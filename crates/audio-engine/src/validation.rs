use std::{
    collections::BTreeMap,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audio::{
        buffer::{AudioRingBuffer, JitterBuffer, JitterPolicy},
        clock::DriftController,
        engine::{AudioProcessor, AudioRuntime, AudioSink, RuntimeSettings},
        format::{db_to_linear, AudioBlock, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        graph::{
            NodeLayout, NodeProperties, NodeSpec, NodeType, PluginInstanceId, RecordingFormat,
            RuntimeState, RvcInstanceId,
        },
        meter::measure,
        resampler::to_internal,
    },
    commands::tauri_commands,
    config::{
        model::{ApplicationSettings, LimiterDefaults, PersistedConfiguration},
        persistence::ConfigStore,
    },
    controller::GraphController,
    edge::{
        control::EdgeControlMessage,
        receiver::{EdgeHub, EdgeStreamState},
    },
    events::{BackendEvent, BackendEventMonitor},
    nodes::input::replay::ReplayTap,
    plugins::{
        builtins::{
            compressor::COMPRESSOR_PLUGIN_ID, equalizer::EQUALIZER_PLUGIN_ID, gate::GATE_PLUGIN_ID,
            limiter::LIMITER_PLUGIN_ID, noise_suppressor::NOISE_SUPPRESSOR_PLUGIN_ID,
        },
        create_default_plugin_manager,
        instance::PluginPrepareContext,
        package::{discover_process_plugins, load_package_manifest},
        shared_audio::SharedAudioRing,
    },
    recording::session::{ManagedRecorder, MIN_RECORDED_SECONDS},
    recording::{
        codecs::{decode_audio_file, encode_audio_file},
        trim::{export_trimmed, TrimRange},
        waveform::get_waveform,
    },
    registry::device_history::{DeviceConnectionState, DeviceUsage},
    replay::{
        cache::{ReplayCacheConfig, ReplayCacheManager},
        folder_player::scan_folder,
        state::ReplayPlaybackState,
    },
    routing::validator::CyclePolicy,
    rvc::adapter::{PassthroughRvcBackend, RvcNodeProcessor, RvcState},
    shortcut::hotkey_test::HotkeyTestSession,
    windows::{
        devices::{list_devices, AudioDeviceInfo, DeviceDirection},
        process_watcher::{list_audio_processes, list_processes},
        wasapi_capture::start_microphone_capture,
        wasapi_render::start_render,
        worker_status::DeviceWorkerState,
    },
    Backend,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationParameters {
    pub block_frames: usize,
    pub meter_fps: u32,
    pub source_sample_rate: u32,
    pub source_channels: u16,
    pub signal_amplitude: f32,
    pub signal_frequency_hz: f32,
    pub duration_ms: u32,
    pub fixture_duration_ms: u32,
    pub ring_capacity_samples: usize,
    pub waveform_buckets: usize,
    pub limiter_ceiling_db: f32,
    pub limiter_lookahead_ms: f32,
    pub limiter_release_ms: f32,
    pub run_system_probes: bool,
    pub run_process_plugin_probe: bool,
    pub run_vb_cable_probe: bool,
    pub vb_cable_probe_duration_ms: u32,
}

impl Default for ValidationParameters {
    fn default() -> Self {
        Self {
            block_frames: 480,
            meter_fps: 30,
            source_sample_rate: 44_100,
            source_channels: 1,
            signal_amplitude: 1.2,
            signal_frequency_hz: 440.0,
            duration_ms: 250,
            fixture_duration_ms: 10_000,
            ring_capacity_samples: 4096,
            waveform_buckets: 32,
            limiter_ceiling_db: -1.0,
            limiter_lookahead_ms: 3.0,
            limiter_release_ms: 100.0,
            run_system_probes: true,
            run_process_plugin_probe: true,
            run_vb_cable_probe: true,
            vb_cable_probe_duration_ms: 1_200,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Passed,
    Warning,
    Failed,
    Skipped,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCaseResult {
    pub id: String,
    pub category: String,
    pub name: String,
    pub status: ValidationStatus,
    pub duration_ms: u64,
    pub detail: String,
    #[serde(default)]
    pub metrics: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationSummary {
    pub total: usize,
    pub passed: usize,
    pub warnings: usize,
    pub failed: usize,
    pub skipped: usize,
    pub manual: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationEnvironment {
    pub os: String,
    pub architecture: String,
    pub backend_version: String,
    pub internal_sample_rate: u32,
    pub internal_channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRun {
    pub run_id: String,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub parameters: ValidationParameters,
    pub environment: ValidationEnvironment,
    pub summary: ValidationSummary,
    pub results: Vec<ValidationCaseResult>,
}

struct CaseOutcome {
    status: ValidationStatus,
    detail: String,
    metrics: BTreeMap<String, Value>,
}

impl CaseOutcome {
    fn passed(detail: impl Into<String>) -> Self {
        Self {
            status: ValidationStatus::Passed,
            detail: detail.into(),
            metrics: BTreeMap::new(),
        }
    }

    fn warning(detail: impl Into<String>) -> Self {
        Self {
            status: ValidationStatus::Warning,
            detail: detail.into(),
            metrics: BTreeMap::new(),
        }
    }

    fn skipped(detail: impl Into<String>) -> Self {
        Self {
            status: ValidationStatus::Skipped,
            detail: detail.into(),
            metrics: BTreeMap::new(),
        }
    }

    fn metric(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metrics.insert(key.into(), value.into());
        self
    }
}

pub fn run_validation(parameters: ValidationParameters) -> ValidationRun {
    let suite_started = Instant::now();
    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut results = Vec::new();

    let parameter_errors = validate_parameters(&parameters);
    if parameter_errors.is_empty() {
        results.push(execute_case(
            "settings.validation",
            "設定・構成",
            "アプリケーション設定の範囲検証",
            || validate_application_settings(&parameters),
        ));
        results.push(execute_case(
            "config.persistence",
            "設定・構成",
            "設定ファイルの保存・再読込",
            || validate_config_persistence(&parameters),
        ));
        results.push(execute_case(
            "commands.facade",
            "設定・構成",
            "バックエンドコマンドFacade",
            validate_command_facade,
        ));
        results.push(execute_case(
            "v02.registry_graph",
            "v0.1.0 登録・配置",
            "Registered NodeとGraph Instanceの分離",
            || validate_v02_registry_graph(&parameters),
        ));
        results.push(execute_case(
            "v02.device_lifecycle",
            "v0.1.0 登録・配置",
            "デバイス自動登録・切断保持・用途別設定",
            validate_v02_device_lifecycle,
        ));
        results.push(execute_case(
            "v02.routing_traversal",
            "v0.1.0 Routing",
            "結線候補・動的Port・経路探索",
            validate_v02_routing_traversal,
        ));
        results.push(execute_case(
            "audio.resampling",
            "オーディオコア",
            "48 kHz・Stereoへの変換",
            || validate_resampling(&parameters),
        ));
        results.push(execute_case(
            "audio.meter",
            "オーディオコア",
            "Peak・RMS・クリッピング検出",
            || validate_meter(&parameters),
        ));
        results.push(execute_case(
            "audio.ring",
            "オーディオコア",
            "ロックフリーAudio Ring",
            || validate_audio_ring(&parameters),
        ));
        results.push(execute_case(
            "audio.jitter",
            "オーディオコア",
            "Jitter Buffer・Drift補正",
            || validate_jitter_and_drift(&parameters),
        ));
        results.push(execute_case(
            "audio.replay",
            "オーディオコア",
            "Replay Tap",
            || validate_replay(&parameters),
        ));
        results.push(execute_case(
            "audio.runtime",
            "オーディオコア",
            "Audio Graphランタイム起動・停止",
            || validate_runtime(&parameters),
        ));
        results.push(execute_case(
            "routing.graph",
            "グラフ・ルーティング",
            "分岐・Multi Input・出力ルート",
            validate_graph_routing,
        ));
        results.push(execute_case(
            "routing.loop",
            "グラフ・ルーティング",
            "明示Graph Loop警告",
            validate_loop_detection,
        ));
        results.push(execute_case(
            "plugins.registry",
            "Transform Plugin",
            "Plugin登録・Manifest・探索",
            validate_plugin_registry,
        ));
        results.push(execute_case(
            "plugins.limiter",
            "Transform Plugin",
            "Built-in Limiter処理",
            || validate_limiter(&parameters),
        ));
        results.push(execute_case(
            "plugins.builtin_dsp",
            "Transform Plugin",
            "組み込みDSPチェーン（T04-E-01）",
            || validate_builtin_dsp_chain(&parameters),
        ));
        results.push(execute_case(
            "shortcuts.hotkey_test_mode",
            "Shortcut",
            "Hotkeyテストモード模擬発火（T05）",
            validate_hotkey_test_mode,
        ));
        results.push(execute_case(
            "plugins.shared_audio",
            "Transform Plugin",
            "共有メモリAudio Ring",
            || validate_shared_audio(&parameters),
        ));
        results.push(execute_case(
            "edge.pcm",
            "Edge入力",
            "Edge PCM変換・Heartbeat",
            || validate_edge_pcm(&parameters),
        ));
        results.push(execute_case(
            "v02.backend_events",
            "v0.1.0 Backend Event",
            "Edge安定再接続・状態差分Event",
            validate_v02_backend_events,
        ));
        results.push(execute_case(
            "recording.codecs",
            "録音・ファイル",
            "WAV・FLAC・MP3 Round Trip",
            || validate_codecs(&parameters),
        ));
        results.push(execute_case(
            "recording.waveform_trim",
            "録音・ファイル",
            "波形生成・非破壊Trim出力",
            || validate_waveform_and_trim(&parameters),
        ));
        results.push(execute_case(
            "rvc.compatibility",
            "RVC互換層",
            "Legacy RVC Adapter状態遷移",
            || validate_rvc_adapter(&parameters),
        ));
        results.push(execute_case(
            "v02.replay_record_profile",
            "v0.1.0 Replay・録音・Profile",
            "Replay・Cache・Recorder・Profile統合",
            || validate_v02_replay_record_profile(&parameters),
        ));
        results.push(execute_case(
            "v02.rvc_process_e2e",
            "v0.1.0 Process Plugin",
            "10秒音声RVC Pass-through・録音E2E",
            || validate_v02_rvc_process_e2e(&parameters),
        ));
        results.push(execute_case(
            "v02.tauri_frontend",
            "v0.1.0 Tauri Frontend",
            "Tauri 2・v11 Backend Bridge契約",
            validate_tauri_frontend_contract,
        ));
        results.push(execute_case(
            "windows.devices",
            "Windows・実機",
            "WASAPIデバイス列挙",
            || validate_windows_devices(&parameters),
        ));
        results.push(execute_case(
            "windows.processes",
            "Windows・実機",
            "Process・Audio Session列挙",
            || validate_windows_processes(&parameters),
        ));
        results.push(execute_case(
            "windows.vb_cable_signal",
            "Windows・実機",
            "VB-CABLE Render→Capture実信号Probe",
            || validate_vb_cable_signal(&parameters),
        ));
        results.push(execute_case(
            "windows.vb_cable_graph_e2e",
            "Windows・実機",
            "生成WAV→Graph→VB-CABLE→Graph→Recorder E2E",
            || validate_vb_cable_graph_e2e(&parameters),
        ));
    } else {
        results.push(ValidationCaseResult {
            id: "parameters.input".into(),
            category: "設定・構成".into(),
            name: "入力パラメータ".into(),
            status: ValidationStatus::Failed,
            duration_ms: 0,
            detail: parameter_errors.join(" / "),
            metrics: BTreeMap::new(),
        });
        results.push(ValidationCaseResult {
            id: "suite.skipped".into(),
            category: "設定・構成".into(),
            name: "自動検証スイート".into(),
            status: ValidationStatus::Skipped,
            duration_ms: 0,
            detail: "入力値を修正してから再実行してください。".into(),
            metrics: BTreeMap::new(),
        });
    }

    append_manual_cases(&mut results);
    let summary = summarize(&results);

    ValidationRun {
        run_id: Uuid::new_v4().to_string(),
        started_at_unix_ms,
        elapsed_ms: suite_started.elapsed().as_millis() as u64,
        parameters,
        environment: ValidationEnvironment {
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            backend_version: env!("CARGO_PKG_VERSION").into(),
            internal_sample_rate: INTERNAL_SAMPLE_RATE,
            internal_channels: INTERNAL_CHANNELS,
        },
        summary,
        results,
    }
}

fn execute_case(
    id: &str,
    category: &str,
    name: &str,
    check: impl FnOnce() -> std::result::Result<CaseOutcome, String>,
) -> ValidationCaseResult {
    let started = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(check));
    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(Ok(outcome)) => ValidationCaseResult {
            id: id.into(),
            category: category.into(),
            name: name.into(),
            status: outcome.status,
            duration_ms,
            detail: outcome.detail,
            metrics: outcome.metrics,
        },
        Ok(Err(message)) => ValidationCaseResult {
            id: id.into(),
            category: category.into(),
            name: name.into(),
            status: ValidationStatus::Failed,
            duration_ms,
            detail: message,
            metrics: BTreeMap::new(),
        },
        Err(_) => ValidationCaseResult {
            id: id.into(),
            category: category.into(),
            name: name.into(),
            status: ValidationStatus::Failed,
            duration_ms,
            detail: "検証処理内で予期しないpanicが発生しました。".into(),
            metrics: BTreeMap::new(),
        },
    }
}

fn validate_parameters(parameters: &ValidationParameters) -> Vec<String> {
    let mut errors = Vec::new();
    if !(64..=4096).contains(&parameters.block_frames) {
        errors.push("Block Framesは64〜4096で指定してください".into());
    }
    if !(1..=120).contains(&parameters.meter_fps) {
        errors.push("Meter FPSは1〜120で指定してください".into());
    }
    if !(8_000..=192_000).contains(&parameters.source_sample_rate) {
        errors.push("入力Sample Rateは8,000〜192,000 Hzで指定してください".into());
    }
    if !matches!(parameters.source_channels, 1 | 2) {
        errors.push("入力Channel数は1または2で指定してください".into());
    }
    if !parameters.signal_amplitude.is_finite()
        || !(0.0..=2.0).contains(&parameters.signal_amplitude)
    {
        errors.push("信号振幅は0.0〜2.0で指定してください".into());
    }
    if !parameters.signal_frequency_hz.is_finite()
        || parameters.signal_frequency_hz < 20.0
        || parameters.signal_frequency_hz >= parameters.source_sample_rate as f32 / 2.0
    {
        errors.push(
            "信号周波数は20 Hz以上かつ入力Sample RateのNyquist周波数未満にしてください".into(),
        );
    }
    if !(20..=1000).contains(&parameters.duration_ms) {
        errors.push("検証信号長は20〜1000 msで指定してください".into());
    }
    if !(1_000..=30_000).contains(&parameters.fixture_duration_ms)
        || parameters.fixture_duration_ms % 10 != 0
    {
        errors.push("E2E音声長は1,000〜30,000 msの10 ms単位で指定してください".into());
    }
    if !(64..=1_000_000).contains(&parameters.ring_capacity_samples) {
        errors.push("Ring容量は64〜1,000,000 samplesで指定してください".into());
    }
    if !(8..=512).contains(&parameters.waveform_buckets) {
        errors.push("波形Bucket数は8〜512で指定してください".into());
    }
    if !parameters.limiter_ceiling_db.is_finite()
        || !(-24.0..=0.0).contains(&parameters.limiter_ceiling_db)
    {
        errors.push("Limiter Ceilingは-24〜0 dBFSで指定してください".into());
    }
    if !parameters.limiter_lookahead_ms.is_finite()
        || !(0.0..=20.0).contains(&parameters.limiter_lookahead_ms)
    {
        errors.push("Limiter Lookaheadは0〜20 msで指定してください".into());
    }
    if !parameters.limiter_release_ms.is_finite()
        || !(5.0..=5000.0).contains(&parameters.limiter_release_ms)
    {
        errors.push("Limiter Releaseは5〜5000 msで指定してください".into());
    }
    if !(250..=5_000).contains(&parameters.vb_cable_probe_duration_ms) {
        errors.push("VB-CABLE Probe長は250〜5,000 msで指定してください".into());
    }
    errors
}

fn validate_application_settings(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let settings = application_settings(parameters);
    settings.validate()?;
    RuntimeSettings {
        block_frames: parameters.block_frames,
        meter_fps: parameters.meter_fps,
    }
    .validate()
    .map_err(|error| error.to_string())?;
    Ok(CaseOutcome::passed("設定値はすべて有効範囲内です。")
        .metric("block_frames", json!(parameters.block_frames))
        .metric("meter_fps", json!(parameters.meter_fps)))
}

fn validate_config_persistence(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let store = ConfigStore::new(workspace.path().join("config/settings.json"));
    let mut config = PersistedConfiguration::default();
    config.settings = application_settings(parameters);
    store.save(&config).map_err(|error| error.to_string())?;
    let loaded = store.load().map_err(|error| error.to_string())?;
    if loaded.settings.block_frames != parameters.block_frames
        || loaded.settings.meter_fps != parameters.meter_fps
    {
        return Err("保存した設定値と再読込結果が一致しません。".into());
    }
    let resolved = store.resolve_relative("plugins");
    if resolved.parent() != Some(store.base_dir().as_path()) {
        return Err("相対Plugin検索パスを設定ファイル基準で解決できません。".into());
    }
    Ok(
        CaseOutcome::passed("一時設定ファイルへの保存と再読込に成功しました。").metric(
            "bytes",
            json!(fs::metadata(store.path()).map_err(|e| e.to_string())?.len()),
        ),
    )
}

fn validate_command_facade() -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let backend = Backend::new(ConfigStore::new(workspace.path().join("backend.json")))
        .map_err(|error| error.to_string())?;
    let settings = tauri_commands::get_settings(&backend).map_err(|error| error.to_string())?;
    let plugins = tauri_commands::list_transform_plugins(&backend);
    if !plugins
        .iter()
        .any(|plugin| plugin.plugin_id == LIMITER_PLUGIN_ID)
    {
        return Err("コマンドFacadeからBuilt-in Limiterを取得できません。".into());
    }
    Ok(
        CaseOutcome::passed("設定取得とTransform Plugin一覧取得に成功しました。")
            .metric("plugins", json!(plugins.len()))
            .metric("block_frames", json!(settings.block_frames)),
    )
}

fn validate_v02_registry_graph(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let fixture = workspace.path().join("registered.wav");
    let samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        parameters.duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude.min(0.5),
    );
    encode_audio_file(&fixture, RecordingFormat::Wav, &samples)
        .map_err(|error| error.to_string())?;
    let config_path = workspace.path().join("registry-config.json");
    let backend =
        Backend::new(ConfigStore::new(&config_path)).map_err(|error| error.to_string())?;
    let definition = tauri_commands::register_fixed_file_input(
        &backend,
        "検証用Registered File".into(),
        fixture,
        false,
    )
    .map_err(|error| error.to_string())?;
    let instance = tauri_commands::add_graph_node(&backend, definition.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::update_node_layout(&backend, NodeLayout::new(instance.id, 120.0, 80.0))
        .map_err(|error| error.to_string())?;
    tauri_commands::update_node_comment(&backend, instance.id, "配置状態の検証".into())
        .map_err(|error| error.to_string())?;
    let duplicate = tauri_commands::duplicate_registered_node(&backend, definition.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::remove_graph_node(&backend, instance.id).map_err(|error| error.to_string())?;
    if backend.registry.get(definition.id).is_err()
        || backend.registry.get(duplicate.id).is_err()
        || !backend.controller.list_nodes().is_empty()
    {
        return Err("Graph Instance削除後のRegistered Node保持状態が不正です。".into());
    }
    tauri_commands::save_current_configuration(&backend).map_err(|error| error.to_string())?;
    drop(backend);
    let reopened =
        Backend::new(ConfigStore::new(&config_path)).map_err(|error| error.to_string())?;
    if reopened.registry.get(definition.id).is_err() || reopened.registry.get(duplicate.id).is_err()
    {
        return Err("Registered Nodeを再起動後に復元できません。".into());
    }
    Ok(
        CaseOutcome::passed("登録定義、Graph配置、Layout、複製、削除、再読込を確認しました。")
            .metric("registered_nodes", json!(reopened.registry.list().len()))
            .metric(
                "graph_instances_after_remove",
                json!(reopened.controller.list_nodes().len()),
            ),
    )
}

fn validation_device(endpoint_id: &str, direction: DeviceDirection) -> AudioDeviceInfo {
    AudioDeviceInfo {
        endpoint_id: endpoint_id.into(),
        friendly_name: endpoint_id.into(),
        direction,
        state: "Active".into(),
        is_default: false,
        is_default_communications: false,
        current_sample_rate: Some(INTERNAL_SAMPLE_RATE),
        channels: Some(INTERNAL_CHANNELS),
        supported_sample_rates: vec![INTERNAL_SAMPLE_RATE],
        user_visible: true,
        user_sort_order: 0,
    }
}

fn validate_v02_device_lifecycle() -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let config_path = workspace.path().join("device-lifecycle.json");
    let backend =
        Backend::new(ConfigStore::new(&config_path)).map_err(|error| error.to_string())?;
    let capture = validation_device("validation-capture", DeviceDirection::Capture);
    let render = validation_device("validation-render", DeviceDirection::Render);
    tauri_commands::apply_device_refresh(
        &backend,
        &tauri_commands::DeviceLists {
            inputs: vec![capture.clone()],
            outputs: vec![render],
        },
        1_000,
    )
    .map_err(|error| error.to_string())?;
    let known = tauri_commands::list_known_devices(&backend);
    let capture_known = known
        .iter()
        .find(|device| device.endpoint_id == "validation-capture")
        .ok_or_else(|| "Captureデバイスが履歴へ追加されませんでした。".to_string())?
        .clone();
    let render_known = known
        .iter()
        .find(|device| device.endpoint_id == "validation-render")
        .ok_or_else(|| "Renderデバイスが履歴へ追加されませんでした。".to_string())?
        .clone();
    if backend.registry.list().len() != 3 {
        return Err("Capture/Renderから3種類の登録定義を生成できませんでした。".into());
    }
    tauri_commands::set_known_device_usage(
        &backend,
        capture_known.app_device_id,
        DeviceUsage::Capture,
        false,
    )
    .map_err(|error| error.to_string())?;
    tauri_commands::apply_device_refresh(
        &backend,
        &tauri_commands::DeviceLists {
            inputs: Vec::new(),
            outputs: Vec::new(),
        },
        2_000,
    )
    .map_err(|error| error.to_string())?;
    if !tauri_commands::list_known_devices(&backend)
        .iter()
        .all(|device| device.connection_state == DeviceConnectionState::Disconnected)
    {
        return Err("切断済みデバイスの履歴状態が不正です。".into());
    }
    tauri_commands::apply_device_refresh(
        &backend,
        &tauri_commands::DeviceLists {
            inputs: vec![capture],
            outputs: Vec::new(),
        },
        3_000,
    )
    .map_err(|error| error.to_string())?;
    let reconnected = tauri_commands::list_known_devices(&backend)
        .into_iter()
        .find(|device| device.endpoint_id == "validation-capture")
        .ok_or_else(|| "Captureデバイスを再接続できませんでした。".to_string())?;
    if reconnected.app_device_id != capture_known.app_device_id
        || reconnected.capture_enabled
        || reconnected.connection_state != DeviceConnectionState::Connected
    {
        return Err("再接続後に安定IDまたは入力用途OFFを維持できませんでした。".into());
    }
    tauri_commands::forget_device(&backend, render_known.app_device_id)
        .map_err(|error| error.to_string())?;
    if tauri_commands::list_known_devices(&backend)
        .iter()
        .any(|device| device.endpoint_id == "validation-render")
    {
        return Err("切断済みRenderデバイスの履歴を削除できませんでした。".into());
    }
    Ok(CaseOutcome::passed(
        "自動ON登録、用途別OFF、切断保持、安定ID再接続、履歴削除を確認しました。",
    )
    .metric(
        "registered_definitions",
        json!(backend.registry.list().len()),
    )
    .metric(
        "known_devices",
        json!(tauri_commands::list_known_devices(&backend).len()),
    ))
}

fn validate_v02_backend_events() -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let backend = Backend::new(ConfigStore::new(workspace.path().join("events.json")))
        .map_err(|error| error.to_string())?;
    let definition = tauri_commands::register_edge_tab_input(
        &backend,
        "Event Edge".into(),
        "event-stable-stream".into(),
    )
    .map_err(|error| error.to_string())?;
    let node = tauri_commands::add_graph_node(&backend, definition.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::start_node(&backend, node.id).map_err(|error| error.to_string())?;
    let stable = backend
        .edge
        .get("event-stable-stream")
        .ok_or_else(|| "Edge安定ストリームがありません。".to_string())?;
    backend.nodes.refresh_external_states_with_processes(&[]);
    let mut monitor = BackendEventMonitor::default();
    monitor.poll(&backend);
    let connected = backend.edge.register("event-stable-stream", 48_000, 2);
    if !Arc::ptr_eq(&stable, &connected) {
        return Err("Edge再接続で安定ストリームが置換されました。".into());
    }
    backend.nodes.refresh_external_states_with_processes(&[]);
    let events = monitor.poll(&backend);
    let reconnected = events.iter().any(|event| {
        matches!(
            event,
            BackendEvent::EdgeTabStateChanged {
                node_id,
                state: RuntimeState::Running,
                reconnected: true,
                ..
            } if *node_id == node.id
        )
    });
    backend.edge.remove("event-stable-stream");
    let retained = backend
        .edge
        .get("event-stable-stream")
        .is_some_and(|stream| Arc::ptr_eq(&stable, &stream));
    backend.nodes.refresh_external_states_with_processes(&[]);
    let disconnected = monitor.poll(&backend).iter().any(|event| {
        matches!(
            event,
            BackendEvent::EdgeTabStateChanged {
                state: RuntimeState::Disconnected,
                ..
            }
        )
    });
    tauri_commands::stop_graph(&backend);
    if !reconnected || !retained || !disconnected {
        return Err("Edge再接続または状態差分Eventが仕様と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("安定Jitter Bufferを保った切断・再接続と状態Eventを確認しました。")
            .metric("reconnect_events", json!(events.len()))
            .metric("stable_stream_retained", json!(retained)),
    )
}

fn validate_v02_routing_traversal() -> std::result::Result<CaseOutcome, String> {
    let controller = GraphController::new();
    let source_a = controller
        .create_node(microphone("Routing A"))
        .map_err(|error| error.to_string())?;
    let source_b = controller
        .create_node(microphone("Routing B"))
        .map_err(|error| error.to_string())?;
    let mix = controller
        .create_node(NodeSpec::new(
            NodeType::MultiInput,
            "Dynamic Multi Input",
            NodeProperties::MultiInput {
                slots: Vec::new(),
                legacy_ducking: None,
                gain_db: 0.0,
                mute: false,
            },
        ))
        .map_err(|error| error.to_string())?;
    let output = controller
        .create_node(monitor("Routing Output"))
        .map_err(|error| error.to_string())?;
    let candidate =
        controller.validate_connection_candidate(source_a.id, mix.id, CyclePolicy::WarningOnly);
    if !candidate.allowed {
        return Err(format!(
            "有効な結線候補が拒否されました: {:?}",
            candidate.reason
        ));
    }
    controller
        .connect_nodes_auto(source_a.id, mix.id)
        .map_err(|error| error.to_string())?;
    let edge_b = controller
        .connect_nodes_auto(source_b.id, mix.id)
        .map_err(|error| error.to_string())?;
    controller
        .set_edge_controls(edge_b.id, -6.0, true)
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes_auto(mix.id, output.id)
        .map_err(|error| error.to_string())?;
    let graph = controller.snapshot();
    let route = crate::routing::traversal::route_to_output(&graph, output.id)
        .map_err(|error| error.to_string())?;
    let duplicate =
        controller.validate_connection_candidate(source_a.id, mix.id, CyclePolicy::WarningOnly);
    if duplicate.allowed
        || graph.nodes[&mix.id].input_ports.len() != 2
        || route.nodes.len() != 4
        || route.edges.len() != 3
    {
        return Err("動的Port、重複拒否、または出力Subgraphが仕様と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("候補検証、2入力動的Port、Gain/Mute同期、出力経路を確認しました。")
            .metric("route_nodes", json!(route.nodes.len()))
            .metric("route_edges", json!(route.edges.len()))
            .metric(
                "dynamic_input_ports",
                json!(graph.nodes[&mix.id].input_ports.len()),
            ),
    )
}

fn validate_resampling(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let input = generate_signal(
        parameters.source_sample_rate,
        parameters.source_channels,
        parameters.duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude,
    );
    let input_frames = input.len() / parameters.source_channels as usize;
    let converted = to_internal(
        &input,
        parameters.source_sample_rate,
        parameters.source_channels,
    );
    let expected_frames = ((input_frames as f64 * INTERNAL_SAMPLE_RATE as f64
        / parameters.source_sample_rate as f64)
        .round()) as usize;
    if converted.format.sample_rate != INTERNAL_SAMPLE_RATE
        || converted.format.channels != INTERNAL_CHANNELS
        || converted.frames().abs_diff(expected_frames) > 1
        || converted.samples.iter().any(|sample| !sample.is_finite())
    {
        return Err("内部Audio Formatへの変換結果が仕様と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("入力信号を48 kHz Float32 Stereoへ変換できました。")
            .metric("input_frames", json!(input_frames))
            .metric("output_frames", json!(converted.frames()))
            .metric("output_samples", json!(converted.samples.len())),
    )
}

fn validate_meter(parameters: &ValidationParameters) -> std::result::Result<CaseOutcome, String> {
    let samples = vec![parameters.signal_amplitude; parameters.block_frames * 2];
    let meter = measure(&samples);
    if (meter.peak - parameters.signal_amplitude).abs() > 0.0001
        || meter.clipped != (parameters.signal_amplitude > 1.0)
        || !meter.rms.is_finite()
    {
        return Err("MeterのPeak・RMS・クリッピング判定が期待値と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("Peak・RMS・クリッピング状態を正しく算出しました。")
            .metric("peak", json!(meter.peak))
            .metric("rms", json!(meter.rms))
            .metric("clipped", json!(meter.clipped)),
    )
}

fn validate_audio_ring(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let ring = AudioRingBuffer::new(parameters.ring_capacity_samples);
    let input_count = parameters
        .ring_capacity_samples
        .saturating_add(parameters.block_frames * 2);
    let input: Vec<f32> = (0..input_count)
        .map(|index| index as f32 / input_count.max(1) as f32)
        .collect();
    let pushed = ring.push_slice(&input);
    let mut output = vec![0.0; input_count];
    let popped = ring.pop_into(&mut output);
    if pushed != ring.capacity_samples()
        || popped != pushed
        || output[..popped] != input[..pushed]
        || ring.len_samples() != 0
    {
        return Err("Audio Ringの容量制限またはFIFO順序が正しくありません。".into());
    }
    Ok(
        CaseOutcome::passed("容量超過時にブロックせず、FIFO順で読み出せました。")
            .metric("capacity_samples", json!(ring.capacity_samples()))
            .metric("accepted_samples", json!(pushed))
            .metric("dropped_samples", json!(input_count - pushed)),
    )
}

fn validate_jitter_and_drift(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let capacity = parameters.ring_capacity_samples.max(64);
    let jitter = JitterBuffer::new(capacity, JitterPolicy::default());
    let first = (capacity / 10).max(1);
    jitter.push(&vec![0.25; first]);
    let mut initial = [1.0; 16];
    if jitter.read_into(&mut initial) != 0 || initial.iter().any(|sample| *sample != 0.0) {
        return Err("開始閾値前のJitter Bufferが無音を返しません。".into());
    }
    jitter.push(&vec![0.5; capacity / 4 + 1]);
    let mut output = [0.0; 16];
    if jitter.read_into(&mut output) == 0 {
        return Err("開始閾値到達後もJitter Bufferを読み出せません。".into());
    }
    let drift = DriftController::new(0.5);
    let centered = drift.correction_ratio(0.5);
    if (centered - 1.0).abs() > 0.0001
        || drift.correction_ratio(0.8) <= 1.0
        || drift.correction_ratio(0.2) >= 1.0
    {
        return Err("Drift補正比率がBuffer充填率へ追従していません。".into());
    }
    Ok(
        CaseOutcome::passed("開始閾値とDrift補正の両方を確認しました。")
            .metric("fill_ratio", json!(jitter.fill_ratio()))
            .metric("center_ratio", json!(centered)),
    )
}

fn validate_replay(parameters: &ValidationParameters) -> std::result::Result<CaseOutcome, String> {
    let tap = ReplayTap::new(1);
    let input = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        parameters.duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude,
    );
    tap.push_interleaved(&input);
    let seconds = parameters.duration_ms as f32 / 1000.0;
    let snapshot = tap.snapshot_last_seconds(seconds);
    if snapshot != input {
        return Err("Replay TapのSnapshotが入力信号と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("直近のAudio信号を欠損なくSnapshotできました。")
            .metric("snapshot_samples", json!(snapshot.len()))
            .metric("seconds", json!(seconds)),
    )
}

fn validate_runtime(parameters: &ValidationParameters) -> std::result::Result<CaseOutcome, String> {
    let controller = GraphController::new();
    let settings = RuntimeSettings {
        block_frames: parameters.block_frames,
        meter_fps: parameters.meter_fps,
    };
    let runtime = AudioRuntime::new(controller.runtime_snapshot(), settings.clone());
    let handle = runtime.start().map_err(|error| error.to_string())?;
    let block_ms =
        (parameters.block_frames as f64 / INTERNAL_SAMPLE_RATE as f64 * 1000.0).ceil() as u64;
    thread::sleep(Duration::from_millis((block_ms * 2).clamp(4, 100)));
    handle.stop();
    if runtime.settings() != settings {
        return Err("Audio Runtimeへ設定値が反映されていません。".into());
    }
    Ok(
        CaseOutcome::passed("Realtime Graph Threadを起動し、安全に停止できました。")
            .metric("block_duration_ms", json!(block_ms)),
    )
}

fn validate_graph_routing() -> std::result::Result<CaseOutcome, String> {
    let controller = GraphController::new();
    let mic_a = controller
        .create_node(microphone("入力A"))
        .map_err(|error| error.to_string())?;
    let mic_b = controller
        .create_node(microphone("入力B"))
        .map_err(|error| error.to_string())?;
    let mut mix = NodeSpec::new(
        NodeType::MultiInput,
        "Multi Input",
        NodeProperties::MultiInput {
            slots: Vec::new(),
            legacy_ducking: None,
            gain_db: 0.0,
            mute: false,
        },
    );
    let second_port = mix
        .add_multi_input_port("input 2")
        .ok_or_else(|| "Multi Input Portを追加できません。".to_string())?;
    let mix = controller
        .create_node(mix)
        .map_err(|error| error.to_string())?;
    let monitor = controller
        .create_node(monitor("Monitor"))
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes(
            mic_a.id,
            mic_a.output_ports[0].id,
            mix.id,
            mix.input_ports[0].id,
        )
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes(mic_b.id, mic_b.output_ports[0].id, mix.id, second_port)
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes(
            mix.id,
            mix.output_ports[0].id,
            monitor.id,
            monitor.input_ports[0].id,
        )
        .map_err(|error| error.to_string())?;
    let report = controller.validate_graph();
    if !report.is_valid() {
        return Err(format!("Graph検証エラー: {:?}", report.issues));
    }
    Ok(
        CaseOutcome::passed("2入力のMixからMonitor出力まで有効なGraphを構築しました。")
            .metric("nodes", json!(controller.list_nodes().len()))
            .metric("edges", json!(controller.list_edges().len())),
    )
}

fn validate_loop_detection() -> std::result::Result<CaseOutcome, String> {
    let controller = GraphController::new();
    let a = controller
        .create_node(rvc("RVC A"))
        .map_err(|error| error.to_string())?;
    let b = controller
        .create_node(rvc("RVC B"))
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes(a.id, a.output_ports[0].id, b.id, b.input_ports[0].id)
        .map_err(|error| error.to_string())?;
    controller
        .connect_nodes(b.id, b.output_ports[0].id, a.id, a.input_ports[0].id)
        .map_err(|error| error.to_string())?;
    let report = controller.validate_graph();
    if !report.is_valid()
        || !report
            .issues
            .iter()
            .any(|issue| issue.code == "explicit_graph_loop")
    {
        return Err("明示LoopをWarningとして検出できません。".into());
    }
    Ok(
        CaseOutcome::passed("Graph Loopを停止エラーではなく警告として検出しました。")
            .metric("issues", json!(report.issues.len())),
    )
}

fn validate_plugin_registry() -> std::result::Result<CaseOutcome, String> {
    let manager = create_default_plugin_manager().map_err(|error| error.to_string())?;
    let descriptor = manager
        .descriptor(LIMITER_PLUGIN_ID)
        .map_err(|error| error.to_string())?;
    let node = manager
        .build_node_spec(LIMITER_PLUGIN_ID, "Limiter", BTreeMap::new())
        .map_err(|error| error.to_string())?;
    if descriptor.input_ports.len() != 1
        || descriptor.output_ports.len() != 1
        || node.node_type != NodeType::TransformPlugin
        || manager
            .validate_parameter(LIMITER_PLUGIN_ID, "ceiling_db", &json!(1.0))
            .is_ok()
    {
        return Err("Built-in PluginのManifestまたはParameter検証が不正です。".into());
    }

    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../dev/examples/rvc-process-plugin-template/plugin.json");
    let package = load_package_manifest(&manifest_path).map_err(|error| error.to_string())?;
    let workspace = ValidationWorkspace::new()?;
    let plugin_dir = workspace.path().join("plugins/rvc");
    fs::create_dir_all(&plugin_dir).map_err(|error| error.to_string())?;
    fs::copy(&manifest_path, plugin_dir.join("plugin.json")).map_err(|error| error.to_string())?;
    let report = discover_process_plugins(&manager, &[workspace.path().join("plugins")]);
    if !report.errors.is_empty()
        || !report
            .loaded
            .iter()
            .any(|id| id == &package.descriptor.plugin_id)
    {
        return Err(format!("Process Plugin探索エラー: {:?}", report.errors));
    }
    Ok(
        CaseOutcome::passed("Built-inとProcess PluginのManifestを検証・登録できました。")
            .metric("registered_plugins", json!(manager.list_plugins().len()))
            .metric("external_plugin_id", json!(package.descriptor.plugin_id)),
    )
}

fn validate_limiter(parameters: &ValidationParameters) -> std::result::Result<CaseOutcome, String> {
    let manager = create_default_plugin_manager().map_err(|error| error.to_string())?;
    let mut plugin_parameters = BTreeMap::new();
    plugin_parameters.insert("ceiling_db".into(), json!(parameters.limiter_ceiling_db));
    plugin_parameters.insert(
        "lookahead_ms".into(),
        json!(parameters.limiter_lookahead_ms),
    );
    plugin_parameters.insert("release_ms".into(), json!(parameters.limiter_release_ms));
    let processor = manager
        .create_instance(
            PluginInstanceId::new(),
            LIMITER_PLUGIN_ID,
            plugin_parameters,
            PluginPrepareContext {
                sample_rate: INTERNAL_SAMPLE_RATE,
                channels: INTERNAL_CHANNELS,
                graph_block_frames: parameters.block_frames,
            },
        )
        .map_err(|error| error.to_string())?;
    processor.start().map_err(|error| error.to_string())?;
    let latency_frames = processor.latency_frames() as usize;
    let blocks = latency_frames / parameters.block_frames + 3;
    let mut output_peak = 0.0f32;
    for _ in 0..blocks {
        let mut block =
            AudioBlock::from_mono(&vec![parameters.signal_amplitude; parameters.block_frames]);
        processor
            .process_in_place(&mut block)
            .map_err(|error| error.to_string())?;
        output_peak = output_peak.max(measure(&block.samples).peak);
    }
    processor.stop().map_err(|error| error.to_string())?;
    let ceiling = db_to_linear(parameters.limiter_ceiling_db);
    let allowed_peak = parameters.signal_amplitude.min(ceiling) + 0.002;
    if !output_peak.is_finite() || output_peak > allowed_peak {
        return Err(format!(
            "Limiter出力Peak {output_peak:.4} が許容値 {allowed_peak:.4} を超えました。"
        ));
    }
    Ok(
        CaseOutcome::passed("指定Ceiling以下へ抑制し、Lookahead遅延を報告しました。")
            .metric("input_peak", json!(parameters.signal_amplitude))
            .metric("output_peak", json!(output_peak))
            .metric("ceiling_linear", json!(ceiling))
            .metric("latency_frames", json!(latency_frames)),
    )
}

fn validate_builtin_dsp_chain(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let manager = create_default_plugin_manager().map_err(|error| error.to_string())?;
    for plugin_id in [
        GATE_PLUGIN_ID,
        EQUALIZER_PLUGIN_ID,
        COMPRESSOR_PLUGIN_ID,
        NOISE_SUPPRESSOR_PLUGIN_ID,
    ] {
        let descriptor = manager
            .descriptor(plugin_id)
            .map_err(|error| error.to_string())?;
        if descriptor.parameters.iter().any(|parameter| {
            parameter.group.is_empty() || parameter.unit.is_empty() || parameter.step.is_none()
        }) {
            return Err(format!(
                "{plugin_id} のManifestにgroup/unit/stepがありません。"
            ));
        }
        let processor = manager
            .create_instance(
                PluginInstanceId::new(),
                plugin_id,
                BTreeMap::new(),
                PluginPrepareContext {
                    sample_rate: INTERNAL_SAMPLE_RATE,
                    channels: INTERNAL_CHANNELS,
                    graph_block_frames: parameters.block_frames.max(64),
                },
            )
            .map_err(|error| error.to_string())?;
        processor.start().map_err(|error| error.to_string())?;
        let frames = parameters.block_frames.max(64);
        let mut mono = vec![0.0f32; frames];
        let omega = 2.0 * std::f32::consts::PI * 1_000.0 / INTERNAL_SAMPLE_RATE as f32;
        for (i, sample) in mono.iter_mut().enumerate() {
            *sample = (omega * i as f32).sin() * 0.1;
        }
        let mut block = AudioBlock::from_mono(&mono);
        processor
            .process_in_place(&mut block)
            .map_err(|error| error.to_string())?;
        if block.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(format!("{plugin_id} の出力が非有限です。"));
        }
        processor.stop().map_err(|error| error.to_string())?;
    }
    Ok(
        CaseOutcome::passed("4種の組み込みDSPがManifest検証と既知sine処理に成功しました。")
            .metric("plugins", json!(4)),
    )
}

fn validate_shared_audio(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let path = workspace.path().join("plugin-audio.pcmmap");
    let mut producer = SharedAudioRing::create(
        &path,
        parameters.ring_capacity_samples,
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
    )
    .map_err(|error| error.to_string())?;
    let mut consumer = SharedAudioRing::open(&path).map_err(|error| error.to_string())?;
    let input: Vec<f32> = (0..parameters.block_frames * 2)
        .map(|index| index as f32 / parameters.block_frames.max(1) as f32)
        .collect();
    let pushed = producer.push(&input);
    let mut output = vec![0.0; input.len()];
    let popped = consumer.pop(&mut output);
    if pushed != input.len() || popped != input.len() || output != input {
        return Err("共有メモリAudio RingのRound Tripが一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("Process Plugin向け共有メモリを相互に読み書きできました。")
            .metric("samples", json!(popped))
            .metric("capacity_samples", json!(consumer.capacity_samples())),
    )
}

fn validate_edge_pcm(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let hub = EdgeHub::default();
    let stream = hub.register(
        "validation-stream",
        parameters.source_sample_rate,
        parameters.source_channels,
    );
    let samples = generate_signal(
        parameters.source_sample_rate,
        parameters.source_channels,
        parameters.duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude,
    );
    let samples_per_frame = 4096 * parameters.source_channels as usize;
    let mut accepted = 0usize;
    for frame in samples.chunks(samples_per_frame) {
        let mut bytes = Vec::with_capacity(frame.len() * 4);
        for sample in frame {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        accepted += hub
            .push_pcm_f32le("validation-stream", &bytes)
            .map_err(|error| error.to_string())?;
    }
    let message = EdgeControlMessage::Hello {
        stream_key: "validation-stream".into(),
        sample_rate: parameters.source_sample_rate,
        channels: parameters.source_channels,
        protocol_version: 0,
        token: None,
        tab_id: None,
        title: None,
        url: None,
    };
    let encoded = serde_json::to_string(&message).map_err(|error| error.to_string())?;
    let _: EdgeControlMessage =
        serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
    if accepted == 0 || stream.state(Duration::from_secs(1)) != EdgeStreamState::Running {
        return Err("Edge PCMを内部Jitter Bufferへ投入できません。".into());
    }
    Ok(
        CaseOutcome::passed("PCM変換、Control JSON、Heartbeat状態を確認しました。")
            .metric("input_samples", json!(samples.len()))
            .metric("accepted_internal_samples", json!(accepted))
            .metric("jitter_fill_ratio", json!(stream.jitter.fill_ratio())),
    )
}

fn validate_codecs(parameters: &ValidationParameters) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let duration_ms = parameters.duration_ms.max(300);
    let samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude.min(0.8),
    );
    let formats = [
        ("wav", RecordingFormat::Wav),
        ("flac", RecordingFormat::Flac),
        ("mp3", RecordingFormat::Mp3),
    ];
    let mut outcome = CaseOutcome::passed("3形式すべてでEncode・Decodeに成功しました。");
    for (extension, format) in formats {
        let path = workspace.path().join(format!("roundtrip.{extension}"));
        encode_audio_file(&path, format, &samples).map_err(|error| error.to_string())?;
        let decoded = decode_audio_file(&path).map_err(|error| error.to_string())?;
        if decoded.samples.is_empty()
            || decoded.format.sample_rate != INTERNAL_SAMPLE_RATE
            || decoded.format.channels != INTERNAL_CHANNELS
            || decoded.samples.iter().any(|sample| !sample.is_finite())
        {
            return Err(format!("{extension}のDecode結果が不正です。"));
        }
        outcome = outcome
            .metric(
                format!("{extension}_bytes"),
                json!(fs::metadata(&path).map_err(|e| e.to_string())?.len()),
            )
            .metric(format!("{extension}_frames"), json!(decoded.frames()));
    }
    Ok(outcome)
}

fn validate_waveform_and_trim(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let duration_ms = parameters.duration_ms.max(200);
    let samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        duration_ms,
        parameters.signal_frequency_hz,
        parameters.signal_amplitude.min(0.8),
    );
    let input = workspace.path().join("source.wav");
    let output = workspace.path().join("trimmed.wav");
    encode_audio_file(&input, RecordingFormat::Wav, &samples).map_err(|error| error.to_string())?;
    let waveform =
        get_waveform(&input, parameters.waveform_buckets).map_err(|error| error.to_string())?;
    let duration_seconds = duration_ms as f64 / 1000.0;
    export_trimmed(
        &input,
        &output,
        TrimRange {
            start_seconds: duration_seconds * 0.25,
            end_seconds: duration_seconds * 0.75,
        },
    )
    .map_err(|error| error.to_string())?;
    let trimmed = decode_audio_file(&output).map_err(|error| error.to_string())?;
    if waveform.is_empty() || trimmed.frames() >= samples.len() / 2 {
        return Err("波形BucketまたはTrim後の長さが期待値と一致しません。".into());
    }
    Ok(
        CaseOutcome::passed("波形Bucketを生成し、指定範囲を別ファイルへ出力しました。")
            .metric("waveform_buckets", json!(waveform.len()))
            .metric("trimmed_frames", json!(trimmed.frames())),
    )
}

fn validate_rvc_adapter(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let latency_frames = (parameters.block_frames / 2) as u32;
    let processor = RvcNodeProcessor::new(Box::new(PassthroughRvcBackend::new(latency_frames)));
    processor.initialize().map_err(|error| error.to_string())?;
    processor
        .load_model("validation-model")
        .map_err(|error| error.to_string())?;
    processor.start().map_err(|error| error.to_string())?;
    let mut block = AudioBlock::from_mono(&vec![0.25; parameters.block_frames]);
    let before = block.samples.clone();
    processor
        .process_in_place(&mut block)
        .map_err(|error| error.to_string())?;
    if block.samples != before || processor.state() != RvcState::Running {
        return Err("RVC Passthrough Adapterの処理または状態が不正です。".into());
    }
    processor
        .set_bypass(true)
        .map_err(|error| error.to_string())?;
    if processor.state() != RvcState::Bypassed {
        return Err("RVC AdapterをBypass状態へ移行できません。".into());
    }
    processor.stop().map_err(|error| error.to_string())?;
    Ok(
        CaseOutcome::passed("初期化、開始、処理、Bypass、停止の状態遷移を確認しました。")
            .metric("latency_frames", json!(processor.latency_frames())),
    )
}

fn validate_v02_replay_record_profile(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    let workspace = ValidationWorkspace::new()?;
    let fixture = workspace.path().join("010_fixture.wav");
    let minimum_recording_ms = (MIN_RECORDED_SECONDS * 1_000.0).ceil() as u32 + 250;
    let fixture_duration_ms = parameters.fixture_duration_ms.max(minimum_recording_ms);
    let samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        fixture_duration_ms,
        parameters.signal_frequency_hz.min(10_000.0),
        parameters.signal_amplitude.min(0.5),
    );
    encode_audio_file(&fixture, RecordingFormat::Wav, &samples)
        .map_err(|error| error.to_string())?;
    let nested = workspace.path().join("nested");
    fs::create_dir_all(&nested).map_err(|error| error.to_string())?;
    fs::copy(&fixture, nested.join("ignored.wav")).map_err(|error| error.to_string())?;
    let folder_files = scan_folder(workspace.path()).map_err(|error| error.to_string())?;
    if folder_files != vec![fixture.clone()] {
        return Err("Folder Replayが直下ファイルだけを列挙していません。".into());
    }

    let cache_manager = ReplayCacheManager::new(workspace.path().join("cache"), Default::default());
    let mut cache_config = ReplayCacheConfig::new("Validation Cache");
    cache_config.max_items = Some(2);
    let cache = cache_manager
        .create_cache(cache_config)
        .map_err(|error| error.to_string())?;
    let oldest = cache_manager
        .add_file(cache.cache_id, &fixture, 1)
        .map_err(|error| error.to_string())?;
    cache_manager
        .add_file(cache.cache_id, &fixture, 2)
        .map_err(|error| error.to_string())?;
    cache_manager
        .add_file(cache.cache_id, &fixture, 3)
        .map_err(|error| error.to_string())?;
    let cache_items = cache_manager
        .list_items(cache.cache_id)
        .map_err(|error| error.to_string())?;
    if cache_items.len() != 2 || oldest.path.exists() {
        return Err("Replay Cacheの最大件数または最古削除が不正です。".into());
    }

    let backend = Backend::new(ConfigStore::new(workspace.path().join("v02-config.json")))
        .map_err(|error| error.to_string())?;
    let source_definition =
        tauri_commands::register_fixed_file_input(&backend, "10秒Replay".into(), fixture, false)
            .map_err(|error| error.to_string())?;
    let recorder_definition = tauri_commands::register_folder_recorder(
        &backend,
        "検証Recorder".into(),
        workspace.path().join("recordings"),
        RecordingFormat::Wav,
        false,
    )
    .map_err(|error| error.to_string())?;
    let source = tauri_commands::add_graph_node(&backend, source_definition.id)
        .map_err(|error| error.to_string())?;
    let recorder = tauri_commands::add_graph_node(&backend, recorder_definition.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::connect_nodes_auto(&backend, source.id, recorder.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::set_gain(&backend, source.id, -3.0).map_err(|error| error.to_string())?;
    let profile = tauri_commands::save_volume_profile(&backend, None, "検証Profile".into())
        .map_err(|error| error.to_string())?;
    tauri_commands::set_gain(&backend, source.id, 6.0).map_err(|error| error.to_string())?;

    tauri_commands::start_graph(&backend).map_err(|error| error.to_string())?;
    tauri_commands::start_recorder(&backend, recorder.id).map_err(|error| error.to_string())?;
    tauri_commands::play_replay(&backend, source.id).map_err(|error| error.to_string())?;
    let recording_deadline = Instant::now()
        + Duration::from_millis(u64::from(fixture_duration_ms).saturating_add(2_000));
    loop {
        let state = tauri_commands::get_recorder_state(&backend, recorder.id)
            .map_err(|error| error.to_string())?;
        if state.duration_seconds > MIN_RECORDED_SECONDS {
            break;
        }
        if Instant::now() >= recording_deadline {
            tauri_commands::stop_graph(&backend);
            return Err(format!(
                "Recorderが保存閾値を超えませんでした: {:.3}秒 / 必要 {:.3}秒超",
                state.duration_seconds, MIN_RECORDED_SECONDS
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    let recording = tauri_commands::stop_recorder(&backend, recorder.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Recorderの出力ファイルがありません。".to_string())?;
    let decoded = decode_audio_file(&recording.path).map_err(|error| error.to_string())?;
    if decoded.samples.is_empty() || !decoded.samples.iter().any(|sample| sample.abs() > 0.01) {
        tauri_commands::stop_graph(&backend);
        return Err("ReplayからRecorderへ有音Audio Blockが到達していません。".into());
    }
    let apply = tauri_commands::apply_volume_profile(&backend, profile.profile_id)
        .map_err(|error| error.to_string())?;
    let replay_state =
        tauri_commands::get_replay_state(&backend, source.id).map_err(|error| error.to_string())?;
    let graph = backend.controller.snapshot();
    tauri_commands::stop_graph(&backend);
    if replay_state.state != ReplayPlaybackState::Stopped
        || graph.nodes[&source.id].properties.gain_mute() != Some((-3.0, false))
    {
        return Err("Profile適用後のGain復元またはReplay停止が不正です。".into());
    }
    Ok(CaseOutcome::passed(
        "非再帰Folder、Cache最古削除、Replay→Recorder、Profile復元を確認しました。",
    )
    .metric("fixture_duration_ms", json!(fixture_duration_ms))
    .metric("fixture_samples", json!(samples.len()))
    .metric("cache_items", json!(cache_items.len()))
    .metric("recorded_samples", json!(decoded.samples.len()))
    .metric("replays_stopped", json!(apply.replays_stopped)))
}

fn validate_v02_rvc_process_e2e(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    if !parameters.run_process_plugin_probe {
        return Ok(CaseOutcome::skipped(
            "Process Plugin Probeが無効なため10秒RVC E2Eを省略しました。",
        ));
    }
    let Some(executable) = find_rvc_passthrough_executable() else {
        return Ok(CaseOutcome::skipped(
            "rvc-passthrough-plugin実行ファイルが見つかりません。起動スクリプトから再実行してください。",
        ));
    };
    let workspace = ValidationWorkspace::new()?;
    let fixture = workspace.path().join("e2e-fixture.wav");
    let source_samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        parameters.fixture_duration_ms,
        parameters.signal_frequency_hz.min(10_000.0),
        parameters.signal_amplitude.min(0.5),
    );
    encode_audio_file(&fixture, RecordingFormat::Wav, &source_samples)
        .map_err(|error| error.to_string())?;
    let source = decode_audio_file(&fixture).map_err(|error| error.to_string())?;
    if source.samples.len() != source_samples.len() {
        return Err("10秒fixtureのEncode・Decode後sample数が一致しません。".into());
    }

    let manifest_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/rvc-passthrough/plugin.json");
    let mut manifest = load_package_manifest(&manifest_path).map_err(|error| error.to_string())?;
    manifest
        .process
        .as_mut()
        .ok_or_else(|| "RVC mock ManifestにProcess設定がありません。".to_string())?
        .executable = executable.clone();
    let package_dir = workspace.path().join("plugins/rvc-passthrough");
    fs::create_dir_all(&package_dir).map_err(|error| error.to_string())?;
    fs::write(
        package_dir.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let manager = create_default_plugin_manager().map_err(|error| error.to_string())?;
    let discovery = discover_process_plugins(&manager, &[package_dir]);
    if !discovery.errors.is_empty()
        || !discovery
            .loaded
            .iter()
            .any(|plugin_id| plugin_id == "voicemixer.rvc.passthrough")
    {
        return Err(format!("RVC mock探索エラー: {:?}", discovery.errors));
    }
    let descriptor = manager
        .descriptor("voicemixer.rvc.passthrough")
        .map_err(|error| error.to_string())?;
    let processor = manager
        .create_instance(
            PluginInstanceId::new(),
            "voicemixer.rvc.passthrough",
            descriptor.default_parameters(),
            PluginPrepareContext {
                sample_rate: INTERNAL_SAMPLE_RATE,
                channels: INTERNAL_CHANNELS,
                graph_block_frames: 480,
            },
        )
        .map_err(|error| error.to_string())?;
    processor.start().map_err(|error| error.to_string())?;
    let recorder = ManagedRecorder::folder(
        workspace.path().join("rvc-recordings"),
        RecordingFormat::Wav,
        false,
    );
    recorder
        .begin_session(true)
        .map_err(|error| error.to_string())?;
    recorder
        .start_manual(true)
        .map_err(|error| error.to_string())?;
    let mut actual = Vec::with_capacity(source.samples.len() + 960);
    for chunk in source.samples.chunks(960) {
        let mut block = AudioBlock::from_interleaved_stereo(chunk.to_vec());
        processor
            .process_in_place(&mut block)
            .map_err(|error| error.to_string())?;
        recorder.write(&block).map_err(|error| error.to_string())?;
        actual.extend_from_slice(&block.samples);
        thread::sleep(Duration::from_millis(2));
    }
    let mut tail = AudioBlock::silence(480);
    processor
        .process_in_place(&mut tail)
        .map_err(|error| error.to_string())?;
    recorder.write(&tail).map_err(|error| error.to_string())?;
    actual.extend_from_slice(&tail.samples);
    processor.stop().map_err(|error| error.to_string())?;
    let recorded = recorder
        .stop_manual()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RVC E2E録音ファイルがありません。".to_string())?;
    let decoded = decode_audio_file(&recorded.path).map_err(|error| error.to_string())?;
    if actual.len() != source.samples.len() + 960
        || actual[..960].iter().any(|sample| *sample != 0.0)
    {
        return Err("RVC mockの1 block遅延または出力sample数が不正です。".into());
    }
    let maximum_error = actual[960..]
        .iter()
        .zip(&source.samples)
        .map(|(actual, expected)| (actual - expected).abs())
        .fold(0.0f32, f32::max);
    if maximum_error >= 0.000_001 || decoded.samples.len() != actual.len() {
        return Err(format!(
            "RVC mock往復誤差または録音sample数が不正です: {maximum_error}"
        ));
    }
    Ok(
        CaseOutcome::passed("生成WAV全体を別Processの共有Ringへ通し、無加工出力を再録音しました。")
            .metric("fixture_duration_ms", json!(parameters.fixture_duration_ms))
            .metric("source_samples", json!(source.samples.len()))
            .metric("recorded_samples", json!(decoded.samples.len()))
            .metric("latency_frames", json!(processor.latency_frames()))
            .metric("maximum_error", json!(maximum_error))
            .metric("plugin_executable", json!(executable)),
    )
}

fn find_rvc_passthrough_executable() -> Option<PathBuf> {
    let executable_name = if cfg!(windows) {
        "rvc-passthrough-plugin.exe"
    } else {
        "rvc-passthrough-plugin"
    };
    let current = std::env::current_exe().ok()?;
    let current_dir = current.parent()?;
    let mut candidates = vec![current_dir.join(executable_name)];
    if current_dir.file_name().and_then(|name| name.to_str()) == Some("deps") {
        if let Some(debug_dir) = current_dir.parent() {
            candidates.push(debug_dir.join(executable_name));
        }
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug")
            .join(executable_name),
    );
    candidates.into_iter().find(|path| path.is_file())
}

fn validate_tauri_frontend_contract() -> std::result::Result<CaseOutcome, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tauri: Value = serde_json::from_slice(
        &fs::read(root.join("tauri.conf.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let frontend_url = tauri
        .pointer("/app/windows/0/url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let html = fs::read_to_string(root.join("../frontend/index.html"))
        .map_err(|error| error.to_string())?;
    let bridge = fs::read_to_string(root.join("../frontend/src/backend/bridge.ts"))
        .map_err(|error| error.to_string())?;
    let client = fs::read_to_string(root.join("../frontend/src/backend/client.ts"))
        .map_err(|error| error.to_string())?;
    let required_commands = [
        "frontend_snapshot",
        "add_graph_node",
        "connect_nodes_auto",
        "play_replay",
        "start_recorder",
        "save_volume_profile",
        "apply_volume_profile",
        "set_known_device_usage",
        "forget_device",
    ];
    let missing: Vec<_> = required_commands
        .iter()
        .filter(|command| !bridge.contains(**command))
        .copied()
        .collect();
    if frontend_url != "index.html"
        || !html.contains("/src/main.ts")
        || !bridge.contains("__TAURI__")
        || !bridge.contains("createBackendEventListener")
        || !client.contains("backend://state-event")
        || !missing.is_empty()
    {
        return Err(format!("Tauri/v11 Bridge契約が不足しています: {missing:?}"));
    }
    Ok(
        CaseOutcome::passed("Tauri Window、v11 HTML、主要Command、Event Bridgeを確認しました。")
            .metric("frontend_url", json!(frontend_url))
            .metric("required_commands", json!(required_commands.len()))
            .metric(
                "runtime_event",
                json!(bridge.contains("audio://runtime-event")),
            )
            .metric(
                "backend_state_event",
                json!(client.contains("backend://state-event")),
            ),
    )
}

fn validate_vb_cable_signal(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    if !parameters.run_system_probes || !parameters.run_vb_cable_probe {
        return Ok(CaseOutcome::skipped(
            "VB-CABLE実信号Probeが無効なため省略しました。",
        ));
    }
    let captures = match list_devices(DeviceDirection::Capture) {
        Ok(devices) => devices,
        Err(error) => {
            return Ok(CaseOutcome::warning(format!(
                "Capture列挙に失敗しました: {error}"
            )))
        }
    };
    let renders = match list_devices(DeviceDirection::Render) {
        Ok(devices) => devices,
        Err(error) => {
            return Ok(CaseOutcome::warning(format!(
                "Render列挙に失敗しました: {error}"
            )))
        }
    };
    let render = renders
        .iter()
        .filter(|device| is_vb_cable_name(&device.friendly_name))
        .min_by_key(|device| vb_cable_name_priority(&device.friendly_name, "cable input"));
    let capture = captures
        .iter()
        .filter(|device| is_vb_cable_name(&device.friendly_name))
        .min_by_key(|device| vb_cable_name_priority(&device.friendly_name, "cable output"));
    let (Some(render), Some(capture)) = (render, capture) else {
        return Ok(CaseOutcome::skipped(
            "VB-CABLEのRender/Captureペアを検出できませんでした。",
        ));
    };

    let frequency_hz = 997.0;
    let amplitude = 0.05;
    let signal = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        parameters.vb_cable_probe_duration_ms,
        frequency_hz,
        amplitude,
    );
    let render_ring = AudioRingBuffer::new(signal.len() + 96_000);
    if render_ring.push_slice(&signal) != signal.len() {
        return Err("VB-CABLE Render Ringへ検証信号を格納できません。".into());
    }
    let capture_capacity = signal.len() + 96_000 * 3;
    let capture_ring = AudioRingBuffer::new(capture_capacity);
    let capture_handle = start_microphone_capture(&capture.endpoint_id, capture_ring.clone())
        .map_err(|error| error.to_string())?;
    capture_handle
        .wait_until_running(Duration::from_secs(3))
        .map_err(|error| error.to_string())?;
    let render_handle =
        start_render(&render.endpoint_id, render_ring).map_err(|error| error.to_string())?;
    render_handle
        .wait_until_running(Duration::from_secs(3))
        .map_err(|error| error.to_string())?;
    thread::sleep(Duration::from_millis(
        parameters.vb_cable_probe_duration_ms as u64 + 750,
    ));
    render_handle.stop();
    capture_handle.stop();
    let mut captured = vec![0.0; capture_ring.len_samples()];
    let captured_count = capture_ring.pop_into(&mut captured);
    captured.truncate(captured_count);
    let peak = captured
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0f32, f32::max);
    let tone_amplitude = estimate_tone_amplitude(&captured, frequency_hz);
    let outcome = if captured.is_empty() || tone_amplitude < 0.005 {
        CaseOutcome::warning(
            "VB-CABLEペアは検出できましたが、短時間Probeで既知信号をCaptureできませんでした。",
        )
    } else {
        CaseOutcome::passed("VB-CABLEへ安全な小振幅信号を出力し、対応Captureで検出しました。")
    };
    Ok(outcome
        .metric("render_device", json!(render.friendly_name))
        .metric("capture_device", json!(capture.friendly_name))
        .metric(
            "probe_duration_ms",
            json!(parameters.vb_cable_probe_duration_ms),
        )
        .metric("captured_samples", json!(captured.len()))
        .metric("captured_peak", json!(peak))
        .metric("detected_tone_amplitude", json!(tone_amplitude)))
}

fn validate_vb_cable_graph_e2e(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    if !parameters.run_system_probes || !parameters.run_vb_cable_probe {
        return Ok(CaseOutcome::skipped(
            "VB-CABLE Graph E2Eが無効なため省略しました。",
        ));
    }
    let captures = list_devices(DeviceDirection::Capture).map_err(|error| error.to_string())?;
    let renders = list_devices(DeviceDirection::Render).map_err(|error| error.to_string())?;
    let render = renders
        .iter()
        .filter(|device| is_vb_cable_name(&device.friendly_name))
        .min_by_key(|device| vb_cable_name_priority(&device.friendly_name, "cable input"));
    let capture = captures
        .iter()
        .filter(|device| is_vb_cable_name(&device.friendly_name))
        .min_by_key(|device| vb_cable_name_priority(&device.friendly_name, "cable output"));
    let (Some(render), Some(capture)) = (render, capture) else {
        return Ok(CaseOutcome::skipped(
            "VB-CABLEのRender/Captureペアを検出できませんでした。",
        ));
    };

    let workspace = ValidationWorkspace::new()?;
    let frequency_hz = 997.0;
    let amplitude = 0.05;
    let minimum_recording_ms = (MIN_RECORDED_SECONDS * 1_000.0).ceil() as u32 + 250;
    let probe_duration_ms = parameters
        .vb_cable_probe_duration_ms
        .max(minimum_recording_ms);
    let fixture_samples = generate_signal(
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        probe_duration_ms,
        frequency_hz,
        amplitude,
    );
    let fixture = workspace.path().join("vb-cable-graph-source.wav");
    encode_audio_file(&fixture, RecordingFormat::Wav, &fixture_samples)
        .map_err(|error| error.to_string())?;
    let backend = Backend::new(ConfigStore::new(workspace.path().join("graph-e2e.json")))
        .map_err(|error| error.to_string())?;

    let source_definition = tauri_commands::register_fixed_file_input(
        &backend,
        "VB-CABLE Graph Source".into(),
        fixture,
        false,
    )
    .map_err(|error| error.to_string())?;
    let recorder_definition = tauri_commands::register_folder_recorder(
        &backend,
        "VB-CABLE Graph Recorder".into(),
        workspace.path().join("recordings"),
        RecordingFormat::Wav,
        false,
    )
    .map_err(|error| error.to_string())?;
    let source = tauri_commands::add_graph_node(&backend, source_definition.id)
        .map_err(|error| error.to_string())?;
    let recorder = tauri_commands::add_graph_node(&backend, recorder_definition.id)
        .map_err(|error| error.to_string())?;
    let capture_node = tauri_commands::create_node(
        &backend,
        NodeSpec::new(
            NodeType::Microphone,
            "VB-CABLE Graph Capture",
            NodeProperties::Microphone {
                endpoint_id: capture.endpoint_id.clone(),
                capture_method: crate::audio::graph::CaptureMethod::Auto,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
    .map_err(|error| error.to_string())?;
    let render_node = tauri_commands::create_node(
        &backend,
        NodeSpec::new(
            NodeType::VirtualDevice,
            "VB-CABLE Graph Render",
            NodeProperties::VirtualDevice {
                endpoint_id: render.endpoint_id.clone(),
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
    .map_err(|error| error.to_string())?;
    tauri_commands::connect_nodes_auto(&backend, source.id, render_node.id)
        .map_err(|error| error.to_string())?;
    tauri_commands::connect_nodes_auto(&backend, capture_node.id, recorder.id)
        .map_err(|error| error.to_string())?;

    tauri_commands::start_node(&backend, capture_node.id).map_err(|error| error.to_string())?;
    wait_for_device_node(&backend, capture_node.id, Duration::from_secs(3))?;
    tauri_commands::start_node(&backend, recorder.id).map_err(|error| error.to_string())?;
    tauri_commands::start_recorder(&backend, recorder.id).map_err(|error| error.to_string())?;
    thread::sleep(Duration::from_millis(350));
    tauri_commands::start_node(&backend, source.id).map_err(|error| error.to_string())?;
    tauri_commands::start_node(&backend, render_node.id).map_err(|error| error.to_string())?;
    wait_for_device_node(&backend, render_node.id, Duration::from_secs(3))?;
    tauri_commands::play_replay(&backend, source.id).map_err(|error| error.to_string())?;
    let recording_deadline =
        Instant::now() + Duration::from_millis(u64::from(probe_duration_ms).saturating_add(2_000));
    loop {
        let state = tauri_commands::get_recorder_state(&backend, recorder.id)
            .map_err(|error| error.to_string())?;
        if state.duration_seconds > MIN_RECORDED_SECONDS {
            break;
        }
        if Instant::now() >= recording_deadline {
            tauri_commands::stop_graph(&backend);
            return Err(format!(
                "Graph Recorderが{MIN_RECORDED_SECONDS}秒を超えませんでした（{:.2}秒）。",
                state.duration_seconds
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = tauri_commands::stop_recorder(&backend, recorder.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Graph Recorderが録音ファイルを生成しませんでした。".to_string())?;
    tauri_commands::stop_graph(&backend);
    let decoded = decode_audio_file(&output.path).map_err(|error| error.to_string())?;
    let peak = decoded
        .samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0f32, f32::max);
    let tone_amplitude = estimate_tone_amplitude(&decoded.samples, frequency_hz);
    let outcome = if decoded.samples.is_empty() || tone_amplitude < 0.003 {
        CaseOutcome::warning(
            "Graph E2E録音は生成されましたが、VB-CABLE経由の既知信号を検出できませんでした。",
        )
    } else {
        CaseOutcome::passed(
            "生成WAVをBackend GraphからVB-CABLEへ流し、別Graph経路の録音で検出しました。",
        )
    };
    Ok(outcome
        .metric("render_device", json!(render.friendly_name))
        .metric("capture_device", json!(capture.friendly_name))
        .metric("fixture_samples", json!(fixture_samples.len()))
        .metric("recorded_samples", json!(decoded.samples.len()))
        .metric("recorded_peak", json!(peak))
        .metric("detected_tone_amplitude", json!(tone_amplitude))
        .metric("probe_duration_ms", json!(probe_duration_ms))
        .metric("recording_path", json!(output.path)))
}

fn wait_for_device_node(
    backend: &Backend,
    node_id: crate::audio::graph::NodeId,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let started = Instant::now();
    loop {
        backend.nodes.refresh_external_states_with_processes(&[]);
        let status = backend
            .nodes
            .device_worker_status(node_id)
            .ok_or_else(|| format!("Node {node_id} にWASAPI workerがありません。"))?;
        if status.state == DeviceWorkerState::Running {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "Node {node_id} のWASAPI workerが{} ms以内に起動しませんでした: {}",
                timeout.as_millis(),
                status.last_error.unwrap_or_else(|| "エラー詳細なし".into())
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn is_vb_cable_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    normalized.contains("vb-audio")
        || normalized.contains("vb-cable")
        || normalized.contains("cable input")
        || normalized.contains("cable output")
}

fn vb_cable_name_priority(name: &str, preferred: &str) -> u8 {
    let normalized = name.to_ascii_lowercase();
    if normalized.starts_with(preferred) {
        0
    } else if normalized.contains(preferred) {
        1
    } else {
        2
    }
}

fn estimate_tone_amplitude(samples: &[f32], frequency_hz: f32) -> f32 {
    let frames = samples.len() / INTERNAL_CHANNELS as usize;
    if frames == 0 {
        return 0.0;
    }
    let mut sine = 0.0f64;
    let mut cosine = 0.0f64;
    for frame in 0..frames {
        let sample = samples[frame * INTERNAL_CHANNELS as usize] as f64;
        let phase = std::f64::consts::TAU * frequency_hz as f64 * frame as f64
            / INTERNAL_SAMPLE_RATE as f64;
        sine += sample * phase.sin();
        cosine += sample * phase.cos();
    }
    (2.0 * (sine * sine + cosine * cosine).sqrt() / frames as f64) as f32
}

fn validate_windows_devices(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    if !parameters.run_system_probes {
        return Ok(CaseOutcome::skipped(
            "System Probeが無効なためデバイス列挙を省略しました。",
        ));
    }
    let inputs = list_devices(DeviceDirection::Capture);
    let outputs = list_devices(DeviceDirection::Render);
    match (inputs, outputs) {
        (Ok(inputs), Ok(outputs)) if !inputs.is_empty() || !outputs.is_empty() => {
            Ok(CaseOutcome::passed("WASAPI Endpointを列挙できました。")
                .metric("capture_devices", json!(inputs.len()))
                .metric("render_devices", json!(outputs.len()))
                .metric(
                    "default_devices",
                    json!(inputs
                        .iter()
                        .chain(outputs.iter())
                        .filter(|d| d.is_default)
                        .count()),
                ))
        }
        (Ok(_), Ok(_)) => Ok(CaseOutcome::warning(
            "WASAPI列挙は成功しましたが、利用可能なEndpointがありません。",
        )),
        (inputs, outputs) => Ok(CaseOutcome::warning(format!(
            "実機環境のため検証を完了できませんでした。入力: {} / 出力: {}",
            inputs
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "成功".into()),
            outputs
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "成功".into())
        ))),
    }
}

fn validate_windows_processes(
    parameters: &ValidationParameters,
) -> std::result::Result<CaseOutcome, String> {
    if !parameters.run_system_probes {
        return Ok(CaseOutcome::skipped(
            "System Probeが無効なためProcess列挙を省略しました。",
        ));
    }
    let processes = list_processes();
    let audio_processes = list_audio_processes();
    if processes.is_empty() {
        return Ok(CaseOutcome::warning(
            "Process列挙結果が0件でした。権限と実行環境を確認してください。",
        ));
    }
    Ok(
        CaseOutcome::passed("ProcessとAudio Sessionを列挙できました。")
            .metric("processes", json!(processes.len()))
            .metric("audio_processes", json!(audio_processes.len())),
    )
}

fn validate_hotkey_test_mode() -> std::result::Result<CaseOutcome, String> {
    let session = HotkeyTestSession::start_simulated().map_err(|error| error.to_string())?;
    session.simulate_fire();
    let events = session.wait_for_events(1, Duration::from_secs(2));
    if events.is_empty() || !events[0].success || events[0].unix_ms <= 0 {
        return Err("Hotkeyテストモードが模擬発火を記録しませんでした。".into());
    }
    let export = session.export();
    Ok(CaseOutcome::passed(
        "模擬発火を時刻付き履歴へ記録し、JSONエクスポートできることを確認しました。",
    )
    .metric("combo_label", json!(export.combo_label))
    .metric("events", json!(events.len()))
    .metric("unix_ms", json!(events[0].unix_ms)))
}

fn append_manual_cases(results: &mut Vec<ValidationCaseResult>) {
    let cases = [
        (
            "hardware.wasapi_capture",
            "Microphone・Endpoint Loopback実音声",
            "録音デバイスの選択と実音声入力が必要です。自動検証ではデバイスを占有しません。",
        ),
        (
            "hardware.process_loopback",
            "PID・EXE・Process Filter実音声",
            "対象アプリが音声を再生している状態で、Whitelist／ExcludeList双方の聴感確認が必要です。",
        ),
        (
            "hardware.audio_render",
            "Monitor・仮想Audio Device出力",
            "VB-CABLEは自動Probe対象です。物理Monitor出力の聴感とユーザー指定Endpointは手動確認が必要です。",
        ),
        (
            "hardware.recovery",
            "USB・Bluetooth切断復旧・スリープ復帰",
            "デバイス切断やWindowsスリープを伴うため、手動シナリオとして扱います。",
        ),
        (
            "hardware.listen_route",
            "「このデバイスを聴く」・暗黙Route",
            "Windows Endpoint Property Storeと実際のOS設定を照合する必要があります。",
        ),
        (
            "hardware.edge_websocket",
            "Edge WebSocket実接続",
            "Edge拡張機能からのHello・Heartbeat・PCM送信を伴うEnd-to-End確認が必要です。",
        ),
        (
            "hardware.rvc_process",
            "実モデルRVC Process Plugin End-to-End",
            "Pass-through mockは自動検証対象です。実モデル、GPU、推論品質の確認は実Plugin完成後に必要です。",
        ),
    ];
    for (id, name, detail) in cases {
        results.push(ValidationCaseResult {
            id: id.into(),
            category: "Windows・実機".into(),
            name: name.into(),
            status: ValidationStatus::Manual,
            duration_ms: 0,
            detail: detail.into(),
            metrics: BTreeMap::new(),
        });
    }
}

fn summarize(results: &[ValidationCaseResult]) -> ValidationSummary {
    let mut summary = ValidationSummary {
        total: results.len(),
        ..ValidationSummary::default()
    };
    for result in results {
        match result.status {
            ValidationStatus::Passed => summary.passed += 1,
            ValidationStatus::Warning => summary.warnings += 1,
            ValidationStatus::Failed => summary.failed += 1,
            ValidationStatus::Skipped => summary.skipped += 1,
            ValidationStatus::Manual => summary.manual += 1,
        }
    }
    summary
}

fn application_settings(parameters: &ValidationParameters) -> ApplicationSettings {
    let mut settings = ApplicationSettings::default();
    settings.block_frames = parameters.block_frames;
    settings.meter_fps = parameters.meter_fps;
    settings.limiter_defaults = LimiterDefaults {
        enabled: false,
        ceiling_db: parameters.limiter_ceiling_db,
        lookahead_ms: parameters.limiter_lookahead_ms,
        release_ms: parameters.limiter_release_ms,
    };
    settings
}

fn generate_signal(
    sample_rate: u32,
    channels: u16,
    duration_ms: u32,
    frequency_hz: f32,
    amplitude: f32,
) -> Vec<f32> {
    let frames = ((sample_rate as u64 * duration_ms as u64) / 1000).max(1) as usize;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    for frame in 0..frames {
        let phase = std::f32::consts::TAU * frequency_hz * frame as f32 / sample_rate as f32;
        let sample = phase.sin() * amplitude;
        for _ in 0..channels {
            samples.push(sample);
        }
    }
    samples
}

fn microphone(name: &str) -> NodeSpec {
    NodeSpec::new(
        NodeType::Microphone,
        name,
        NodeProperties::Microphone {
            endpoint_id: format!("validation-{name}"),
            capture_method: crate::audio::graph::CaptureMethod::Auto,
            gain_db: 0.0,
            mute: false,
        },
    )
}

fn monitor(name: &str) -> NodeSpec {
    NodeSpec::new(
        NodeType::Monitor,
        name,
        NodeProperties::Monitor {
            endpoint_id: format!("validation-{name}"),
            gain_db: 0.0,
            mute: false,
        },
    )
}

fn rvc(name: &str) -> NodeSpec {
    NodeSpec::new(
        NodeType::Rvc,
        name,
        NodeProperties::Rvc {
            instance_id: RvcInstanceId::new(),
            bypass: false,
            auto_bypass_on_error: false,
            gain_db: 0.0,
            mute: false,
        },
    )
}

struct ValidationWorkspace {
    path: PathBuf,
}

impl ValidationWorkspace {
    fn new() -> std::result::Result<Self, String> {
        let path = std::env::temp_dir().join(format!("voicemixer-validation-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ValidationWorkspace {
    fn drop(&mut self) {
        let temp_root = std::env::temp_dir();
        if self.path.starts_with(&temp_root)
            && self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("voicemixer-validation-"))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
