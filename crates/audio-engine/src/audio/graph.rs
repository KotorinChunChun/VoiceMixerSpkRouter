use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::actions::model::{ActionNodeConfig, LogOutputConfig};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            Serialize,
            Deserialize,
            Ord,
            PartialOrd,
            schemars::JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(NodeId);
id_type!(RegisteredNodeId);
id_type!(PortId);
id_type!(EdgeId);
id_type!(PluginInstanceId);
id_type!(GroupProcessorId);
id_type!(FilterRuleId);
id_type!(CacheId);
id_type!(ReplayItemId);
// Kept only so v0.1 configuration and API code can be migrated without
// destroying old graph data. New voice conversion nodes must use
// `TransformPlugin` + `PluginInstanceId`.
id_type!(RvcInstanceId);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    #[default]
    Audio,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConnectionEndpoint {
    pub node_id: NodeId,
    #[serde(default)]
    pub port_id: Option<PortId>,
    pub direction: PortDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PortSpec {
    pub id: PortId,
    pub name: String,
    pub direction: PortDirection,
    #[serde(default)]
    pub signal_kind: SignalKind,
    /// Event types accepted or emitted by an Event port. Audio ports keep this
    /// empty. Event connections require at least one common type.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Stable logical port identifier supplied by a transform plugin manifest.
    /// Core nodes leave this unset. Graph edges continue to use `PortId`.
    #[serde(default)]
    pub plugin_port_key: Option<String>,
    /// Maximum number of graph edges accepted by this concrete port. `None`
    /// means unlimited. Core audio inputs default to one upstream edge.
    #[serde(default)]
    pub max_connections: Option<usize>,
    /// Whether an input must have at least one upstream connection.
    /// Existing graph data predates this field, so missing values remain
    /// required for backwards compatibility.
    #[serde(default = "default_required_port")]
    pub required: bool,
}

fn default_required_port() -> bool {
    true
}

impl PortSpec {
    pub fn input(name: impl Into<String>) -> Self {
        Self {
            id: PortId::new(),
            name: name.into(),
            direction: PortDirection::Input,
            signal_kind: SignalKind::Audio,
            event_types: Vec::new(),
            plugin_port_key: None,
            max_connections: Some(1),
            required: true,
        }
    }

    pub fn output(name: impl Into<String>) -> Self {
        Self {
            id: PortId::new(),
            name: name.into(),
            direction: PortDirection::Output,
            signal_kind: SignalKind::Audio,
            event_types: Vec::new(),
            plugin_port_key: None,
            max_connections: None,
            required: true,
        }
    }

    pub fn plugin(
        key: impl Into<String>,
        name: impl Into<String>,
        direction: PortDirection,
        max_connections: Option<usize>,
    ) -> Self {
        Self {
            id: PortId::new(),
            name: name.into(),
            direction,
            signal_kind: SignalKind::Audio,
            event_types: Vec::new(),
            plugin_port_key: Some(key.into()),
            max_connections,
            required: true,
        }
    }

    pub fn event_input(name: impl Into<String>, event_types: Vec<String>) -> Self {
        Self {
            id: PortId::new(),
            name: name.into(),
            direction: PortDirection::Input,
            signal_kind: SignalKind::Event,
            event_types,
            plugin_port_key: None,
            max_connections: None,
            required: true,
        }
    }

    pub fn event_output(name: impl Into<String>, event_types: Vec<String>) -> Self {
        Self {
            id: PortId::new(),
            name: name.into(),
            direction: PortDirection::Output,
            signal_kind: SignalKind::Event,
            event_types,
            plugin_port_key: None,
            max_connections: None,
            required: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeClass {
    Input,
    Transform,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    TestSignal,
    Microphone,
    EdgeTab,
    ProcessPid,
    ProcessExe,
    EndpointLoopback,
    FileInput,
    FolderReplay,
    ReplayInput,
    MemoryReplay,
    TransformPlugin,
    AudioProcessingGroup,
    /// Legacy compatibility only. New RVC implementations are transform plugins.
    Rvc,
    MultiInput,
    InputSelector,
    Delay,
    ChannelMapper,
    SilenceDetector,
    Action,
    LogOutput,
    Monitor,
    VirtualDevice,
    Recorder,
    ReplayCache,
}

impl NodeType {
    pub fn class(self) -> NodeClass {
        match self {
            Self::TestSignal
            | Self::Microphone
            | Self::EdgeTab
            | Self::ProcessPid
            | Self::ProcessExe
            | Self::EndpointLoopback
            | Self::FileInput
            | Self::FolderReplay
            | Self::ReplayInput => NodeClass::Input,
            Self::MemoryReplay => NodeClass::Input,
            Self::TransformPlugin
            | Self::AudioProcessingGroup
            | Self::Rvc
            | Self::MultiInput
            | Self::InputSelector
            | Self::Delay
            | Self::ChannelMapper
            | Self::SilenceDetector => NodeClass::Transform,
            Self::Action
            | Self::LogOutput
            | Self::Monitor
            | Self::VirtualDevice
            | Self::Recorder
            | Self::ReplayCache => NodeClass::Output,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FolderReplayMode {
    Sequential,
    SelectedFileOnly,
}

impl Default for FolderReplayMode {
    fn default() -> Self {
        Self::Sequential
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReplayMode {
    SelectFromCache,
    LatestFromCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Stopped,
    Starting,
    Running,
    Muted,
    Waiting,
    Disconnected,
    Recovering,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationFilterMode {
    Disabled,
    Whitelist,
    ExcludeList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointCaptureMode {
    EndpointLoopback,
    FilteredProcessComposite,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProcessFilterTarget {
    /// Case-insensitive executable file name, e.g. `Discord.exe`.
    ExeName(String),
    /// Optional full-path target retained for precise matching and migration.
    ExePath(PathBuf),
    /// Runtime-only target. Windows may recycle PIDs after a process exits.
    Pid(u32),
    /// Dedicated Windows system-sounds session classification.
    SystemSounds,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ProcessFilterRule {
    pub id: FilterRuleId,
    pub target: ProcessFilterTarget,
    pub enabled: bool,
}

impl ProcessFilterRule {
    pub fn new(target: ProcessFilterTarget) -> Self {
        Self {
            id: FilterRuleId::new(),
            target,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ApplicationFilter {
    pub mode: ApplicationFilterMode,
    #[serde(default)]
    pub rules: Vec<ProcessFilterRule>,
    /// v0.1 stored full EXE paths only. They continue to match while users
    /// migrate them to explicit rules.
    #[serde(default, rename = "exe_paths")]
    pub legacy_exe_paths: Vec<PathBuf>,
}

impl Default for ApplicationFilter {
    fn default() -> Self {
        Self {
            mode: ApplicationFilterMode::Disabled,
            rules: Vec::new(),
            legacy_exe_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransformFailurePolicy {
    /// Privacy-safe default: a failed transform never leaks the unprocessed input.
    Silence,
    Bypass,
    StopNode,
}

impl Default for TransformFailurePolicy {
    fn default() -> Self {
        Self::Silence
    }
}

/// Audio policy used only while a transform plugin process is being recreated
/// (for example when applying a `restart_required` preset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwitchPolicy {
    /// Privacy-safe default: emit silence until the new process is ready.
    Silence,
    /// Pass the unprocessed input through for the duration of the switch.
    Bypass,
}

impl Default for SwitchPolicy {
    fn default() -> Self {
        Self::Silence
    }
}

impl SwitchPolicy {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Silence => 0,
            Self::Bypass => 1,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Bypass,
            _ => Self::Silence,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    Wav,
    Flac,
    Mp3,
}

impl Default for RecordingFormat {
    fn default() -> Self {
        Self::Wav
    }
}

impl RecordingFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
        }
    }

    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .as_deref()
        {
            Some("wav") => Some(Self::Wav),
            Some("flac") => Some(Self::Flac),
            Some("mp3") => Some(Self::Mp3),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct MixSlot {
    pub port_id: PortId,
    #[serde(default)]
    pub edge_id: Option<EdgeId>,
    pub gain_db: f32,
    pub mute: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestSignalType {
    Sine,
    WhiteNoise,
    PinkNoise,
    LeftRightTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestSignalChannelMode {
    Mono,
    Stereo,
    LeftOnly,
    RightOnly,
    LeftRightAlternating,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TestSignalConfig {
    pub signal_type: TestSignalType,
    pub frequency_hz: f32,
    pub level_db: f32,
    pub channel_mode: TestSignalChannelMode,
    pub enabled: bool,
}

impl Default for TestSignalConfig {
    fn default() -> Self {
        Self {
            signal_type: TestSignalType::Sine,
            frequency_hz: 440.0,
            level_db: -18.0,
            channel_mode: TestSignalChannelMode::Stereo,
            enabled: true,
        }
    }
}

impl TestSignalConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.frequency_hz.is_finite() || !(20.0..=20_000.0).contains(&self.frequency_hz) {
            return Err("test signal frequency must be between 20 and 20000 Hz".into());
        }
        if !self.level_db.is_finite() || !(-120.0..=0.0).contains(&self.level_db) {
            return Err("test signal level must be between -120 and 0 dBFS".into());
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SwitcherMode {
    #[default]
    Manual,
    VoiceActivity,
    RoundRobin,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SwitcherManualResume {
    #[default]
    RestartTimer,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SwitcherInputConfig {
    pub port_id: PortId,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct InputSelectorConfig {
    pub active_port_id: Option<PortId>,
    pub mode: SwitcherMode,
    pub default_port_id: Option<PortId>,
    pub silence_threshold_db: f32,
    pub silence_seconds: f64,
    pub detection_seconds: f64,
    pub interval_seconds: f64,
    pub fade_seconds: f64,
    pub manual_resume: SwitcherManualResume,
    pub inputs: Vec<SwitcherInputConfig>,
    /// 同じ設定Portを再選択した操作も実行時へ届ける世代番号。
    pub selection_revision: u64,
    /// 自動切替の明示再開要求。保存しても再開時は通常の初期化となる。
    pub resume_revision: u64,
}

impl Default for InputSelectorConfig {
    fn default() -> Self {
        Self {
            active_port_id: None,
            mode: SwitcherMode::Manual,
            default_port_id: None,
            silence_threshold_db: -48.0,
            silence_seconds: 600.0,
            detection_seconds: 0.1,
            interval_seconds: 30.0,
            fade_seconds: 0.0,
            manual_resume: SwitcherManualResume::RestartTimer,
            inputs: Vec::new(),
            selection_revision: 0,
            resume_revision: 0,
        }
    }
}

impl InputSelectorConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.silence_threshold_db.is_finite()
            || !(-120.0..=0.0).contains(&self.silence_threshold_db)
        {
            return Err("無音閾値は-120〜0 dBFSで設定してください".into());
        }
        for (name, value) in [
            ("無音時間", self.silence_seconds),
            ("検知時間", self.detection_seconds),
            ("巡回時間", self.interval_seconds),
            ("フェード時間", self.fade_seconds),
        ] {
            if !value.is_finite() || !(0.0..=86400.0).contains(&value) {
                return Err(format!("{name}は0〜86400秒で設定してください"));
            }
        }
        if self.interval_seconds <= 0.0 {
            return Err("巡回時間は0秒より大きくしてください".into());
        }
        let mut ports = std::collections::HashSet::new();
        for input in &self.inputs {
            if !ports.insert(input.port_id) {
                return Err("入力設定が重複しています".into());
            }
            if !input.gain_db.is_finite() || !(-120.0..=24.0).contains(&input.gain_db) {
                return Err("入力ゲインは-120〜24 dBで設定してください".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DelayConfig {
    pub delay_ms: f32,
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self { delay_ms: 0.0 }
    }
}

impl DelayConfig {
    pub const MAX_DELAY_MS: f32 = 5_000.0;

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.delay_ms.is_finite() || !(0.0..=Self::MAX_DELAY_MS).contains(&self.delay_ms) {
            return Err("delay must be between 0 and 5000 ms".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMapPreset {
    StereoToMono,
    MonoToStereo,
    LeftToMono,
    RightToMono,
    SwapLeftRight,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChannelMapEntry {
    pub source_channel: usize,
    pub destination_channel: usize,
    pub gain: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChannelPortMapEntry {
    pub source_port_id: PortId,
    pub source: ChannelSource,
    pub destination_channel: usize,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChannelInputMode {
    #[default]
    Single,
    TwoInputs,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSource {
    Left,
    Right,
    #[default]
    Average,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ChannelMapperConfig {
    pub preset: ChannelMapPreset,
    #[serde(default)]
    pub custom_map: Vec<ChannelMapEntry>,
    pub input_mode: ChannelInputMode,
    pub port_map: Vec<ChannelPortMapEntry>,
    pub left_gain_db: f32,
    pub right_gain_db: f32,
    pub left_mute: bool,
    pub right_mute: bool,
}

impl Default for ChannelMapperConfig {
    fn default() -> Self {
        Self {
            preset: ChannelMapPreset::StereoToMono,
            custom_map: Vec::new(),
            input_mode: ChannelInputMode::Single,
            port_map: Vec::new(),
            left_gain_db: 0.0,
            right_gain_db: 0.0,
            left_mute: false,
            right_mute: false,
        }
    }
}

impl ChannelMapperConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        for value in [self.left_gain_db, self.right_gain_db]
            .into_iter()
            .chain(self.port_map.iter().map(|m| m.gain_db))
        {
            if !value.is_finite() || !(-120.0..=24.0).contains(&value) {
                return Err("チャンネルゲインは-120〜24 dBで設定してください".into());
            }
        }
        for entry in &self.port_map {
            if entry.destination_channel >= 2 {
                return Err("出力チャンネルはL(0)またはR(1)です".into());
            }
        }
        for entry in &self.custom_map {
            if entry.source_channel >= 2 || entry.destination_channel >= 2 {
                return Err("channel map currently supports channel indexes 0 and 1".into());
            }
            if !entry.gain.is_finite() {
                return Err("channel map gain must be finite".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DuckingConfig {
    pub enabled: bool,
    pub trigger_port_id: PortId,
    pub target_port_id: PortId,
    pub threshold_db: f32,
    pub reduction_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl DuckingConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.trigger_port_id == self.target_port_id {
            return Err("ducking trigger and target ports must differ".into());
        }
        if !self.threshold_db.is_finite() || !(-120.0..=0.0).contains(&self.threshold_db) {
            return Err("ducking threshold must be between -120 and 0 dBFS".into());
        }
        if !self.reduction_db.is_finite() || !(-120.0..=0.0).contains(&self.reduction_db) {
            return Err("ducking reduction must be between -120 and 0 dB".into());
        }
        if !self.attack_ms.is_finite() || !(0.0..=10_000.0).contains(&self.attack_ms) {
            return Err("ducking attack must be between 0 and 10000 ms".into());
        }
        if !self.release_ms.is_finite() || !(0.0..=10_000.0).contains(&self.release_ms) {
            return Err("ducking release must be between 0 and 10000 ms".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SilenceDetectorConfig {
    pub threshold_db: f32,
    pub duration_ms: u64,
    pub resume_threshold_db: f32,
    pub cooldown_ms: u64,
}

impl Default for SilenceDetectorConfig {
    fn default() -> Self {
        Self {
            threshold_db: -48.0,
            duration_ms: 10_000,
            resume_threshold_db: -42.0,
            cooldown_ms: 3_000,
        }
    }
}

impl SilenceDetectorConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.threshold_db.is_finite() || !(-120.0..=0.0).contains(&self.threshold_db) {
            return Err("silence threshold must be between -120 and 0 dBFS".into());
        }
        if !self.resume_threshold_db.is_finite()
            || !(-120.0..=0.0).contains(&self.resume_threshold_db)
        {
            return Err("silence resume threshold must be between -120 and 0 dBFS".into());
        }
        if self.resume_threshold_db <= self.threshold_db {
            return Err("silence resume threshold must be greater than silence threshold".into());
        }
        const MAX_INTERVAL_MS: u64 = 86_400_000;
        if self.duration_ms > MAX_INTERVAL_MS || self.cooldown_ms > MAX_INTERVAL_MS {
            return Err("silence duration and cooldown must not exceed 24 hours".into());
        }
        Ok(())
    }
}

/// Windows入力EndpointのCapture方式。
///
/// `Auto`は低遅延かつ他アプリと共存しやすい方式から順に試行し、
/// 明示指定時は別方式へ暗黙に切り替えない。
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    #[default]
    Auto,
    WasapiSharedEvent,
    WasapiSharedPolling,
    WasapiExclusiveEvent,
    WasapiExclusivePolling,
    WinMmWaveIn,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeProperties {
    TestSignal {
        config: TestSignalConfig,
        gain_db: f32,
        mute: bool,
    },
    Microphone {
        endpoint_id: String,
        #[serde(default)]
        capture_method: CaptureMethod,
        gain_db: f32,
        mute: bool,
    },
    EdgeTab {
        stream_key: String,
        gain_db: f32,
        mute: bool,
    },
    ProcessPid {
        pid: u32,
        include_process_tree: bool,
        gain_db: f32,
        mute: bool,
    },
    ProcessExe {
        exe_path: PathBuf,
        gain_db: f32,
        mute: bool,
    },
    EndpointLoopback {
        endpoint_id: String,
        filter: ApplicationFilter,
        capture_mode: EndpointCaptureMode,
        gain_db: f32,
        mute: bool,
    },
    FileInput {
        path: PathBuf,
        looped: bool,
        #[serde(default)]
        loop_wait_ms: u64,
        gain_db: f32,
        mute: bool,
    },
    FolderReplay {
        path: PathBuf,
        #[serde(default)]
        mode: FolderReplayMode,
        #[serde(default)]
        selected_file: Option<PathBuf>,
        gain_db: f32,
        mute: bool,
    },
    ReplayInput {
        source_node_id: Option<NodeId>,
        seconds: f32,
        gain_db: f32,
        mute: bool,
    },
    MemoryReplay {
        cache_id: CacheId,
        mode: MemoryReplayMode,
        #[serde(default)]
        selected_item_id: Option<ReplayItemId>,
        gain_db: f32,
        mute: bool,
    },
    TransformPlugin {
        plugin_id: String,
        instance_id: PluginInstanceId,
        #[serde(default)]
        parameters: BTreeMap<String, Value>,
        #[serde(default)]
        bypass: bool,
        #[serde(default)]
        failure_policy: TransformFailurePolicy,
        #[serde(default)]
        switch_policy: SwitchPolicy,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    /// A fixed one-input/one-output serial sub-flow. Members are not graph
    /// nodes and therefore cannot be wired independently or nested.
    AudioProcessingGroup {
        #[serde(default)]
        flow: Vec<GroupProcessorSpec>,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    /// Legacy compatibility only. New RVC nodes are `TransformPlugin` nodes.
    Rvc {
        instance_id: RvcInstanceId,
        bypass: bool,
        auto_bypass_on_error: bool,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    MultiInput {
        slots: Vec<MixSlot>,
        /// Read-only compatibility payload for v0.4.1 and earlier profiles.
        /// It is consumed by the profile/config loader before the graph reaches
        /// the controller and is deliberately never written back out.
        #[serde(default, rename = "ducking", skip_serializing)]
        legacy_ducking: Option<DuckingConfig>,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    InputSelector {
        config: InputSelectorConfig,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    Delay {
        config: DelayConfig,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    ChannelMapper {
        config: ChannelMapperConfig,
        #[serde(default)]
        gain_db: f32,
        #[serde(default)]
        mute: bool,
    },
    SilenceDetector {
        config: SilenceDetectorConfig,
    },
    Action {
        config: ActionNodeConfig,
    },
    LogOutput {
        config: LogOutputConfig,
    },
    Monitor {
        endpoint_id: String,
        gain_db: f32,
        mute: bool,
    },
    VirtualDevice {
        endpoint_id: String,
        gain_db: f32,
        mute: bool,
    },
    Recorder {
        path: PathBuf,
        #[serde(default)]
        format: RecordingFormat,
        /// 旧構成では1つの出力ファイルを保存していた。現行のフォルダ
        /// recorders generate a new timestamped file for every recording.
        #[serde(default)]
        directory_mode: bool,
        #[serde(default)]
        auto_record: bool,
        gain_db: f32,
        mute: bool,
    },
    ReplayCache {
        cache_id: CacheId,
        gain_db: f32,
        mute: bool,
    },
}

/// Persisted settings for an individual group member. This mirrors the
/// configurable part of a TransformPlugin while retaining a separate ID
/// namespace from graph nodes and registered definitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct GroupProcessorSpec {
    pub id: GroupProcessorId,
    pub display_name: String,
    pub plugin_id: String,
    pub instance_id: PluginInstanceId,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub failure_policy: TransformFailurePolicy,
    #[serde(default)]
    pub switch_policy: SwitchPolicy,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
}

impl NodeProperties {
    pub fn node_type(&self) -> NodeType {
        match self {
            Self::TestSignal { .. } => NodeType::TestSignal,
            Self::Microphone { .. } => NodeType::Microphone,
            Self::EdgeTab { .. } => NodeType::EdgeTab,
            Self::ProcessPid { .. } => NodeType::ProcessPid,
            Self::ProcessExe { .. } => NodeType::ProcessExe,
            Self::EndpointLoopback { .. } => NodeType::EndpointLoopback,
            Self::FileInput { .. } => NodeType::FileInput,
            Self::FolderReplay { .. } => NodeType::FolderReplay,
            Self::ReplayInput { .. } => NodeType::ReplayInput,
            Self::MemoryReplay { .. } => NodeType::MemoryReplay,
            Self::TransformPlugin { .. } => NodeType::TransformPlugin,
            Self::AudioProcessingGroup { .. } => NodeType::AudioProcessingGroup,
            Self::Rvc { .. } => NodeType::Rvc,
            Self::MultiInput { .. } => NodeType::MultiInput,
            Self::InputSelector { .. } => NodeType::InputSelector,
            Self::Delay { .. } => NodeType::Delay,
            Self::ChannelMapper { .. } => NodeType::ChannelMapper,
            Self::SilenceDetector { .. } => NodeType::SilenceDetector,
            Self::Action { .. } => NodeType::Action,
            Self::LogOutput { .. } => NodeType::LogOutput,
            Self::Monitor { .. } => NodeType::Monitor,
            Self::VirtualDevice { .. } => NodeType::VirtualDevice,
            Self::Recorder { .. } => NodeType::Recorder,
            Self::ReplayCache { .. } => NodeType::ReplayCache,
        }
    }

    pub fn gain_mute(&self) -> Option<(f32, bool)> {
        match self {
            Self::TestSignal { gain_db, mute, .. }
            | Self::Microphone { gain_db, mute, .. }
            | Self::EdgeTab { gain_db, mute, .. }
            | Self::ProcessPid { gain_db, mute, .. }
            | Self::ProcessExe { gain_db, mute, .. }
            | Self::EndpointLoopback { gain_db, mute, .. }
            | Self::FileInput { gain_db, mute, .. }
            | Self::FolderReplay { gain_db, mute, .. }
            | Self::ReplayInput { gain_db, mute, .. }
            | Self::MemoryReplay { gain_db, mute, .. }
            | Self::TransformPlugin { gain_db, mute, .. }
            | Self::AudioProcessingGroup { gain_db, mute, .. }
            | Self::Rvc { gain_db, mute, .. }
            | Self::MultiInput { gain_db, mute, .. }
            | Self::InputSelector { gain_db, mute, .. }
            | Self::Delay { gain_db, mute, .. }
            | Self::ChannelMapper { gain_db, mute, .. }
            | Self::Monitor { gain_db, mute, .. }
            | Self::VirtualDevice { gain_db, mute, .. }
            | Self::Recorder { gain_db, mute, .. }
            | Self::ReplayCache { gain_db, mute, .. } => Some((*gain_db, *mute)),
            Self::SilenceDetector { .. } | Self::Action { .. } | Self::LogOutput { .. } => None,
        }
    }

    pub fn set_gain_db(&mut self, value: f32) -> bool {
        match self {
            Self::TestSignal { gain_db, .. }
            | Self::Microphone { gain_db, .. }
            | Self::EdgeTab { gain_db, .. }
            | Self::ProcessPid { gain_db, .. }
            | Self::ProcessExe { gain_db, .. }
            | Self::EndpointLoopback { gain_db, .. }
            | Self::FileInput { gain_db, .. }
            | Self::FolderReplay { gain_db, .. }
            | Self::ReplayInput { gain_db, .. }
            | Self::MemoryReplay { gain_db, .. }
            | Self::TransformPlugin { gain_db, .. }
            | Self::AudioProcessingGroup { gain_db, .. }
            | Self::Rvc { gain_db, .. }
            | Self::MultiInput { gain_db, .. }
            | Self::InputSelector { gain_db, .. }
            | Self::Delay { gain_db, .. }
            | Self::ChannelMapper { gain_db, .. }
            | Self::Monitor { gain_db, .. }
            | Self::VirtualDevice { gain_db, .. }
            | Self::Recorder { gain_db, .. }
            | Self::ReplayCache { gain_db, .. } => {
                *gain_db = value;
                true
            }
            Self::SilenceDetector { .. } | Self::Action { .. } | Self::LogOutput { .. } => false,
        }
    }

    pub fn set_mute(&mut self, value: bool) -> bool {
        match self {
            Self::TestSignal { mute, .. }
            | Self::Microphone { mute, .. }
            | Self::EdgeTab { mute, .. }
            | Self::ProcessPid { mute, .. }
            | Self::ProcessExe { mute, .. }
            | Self::EndpointLoopback { mute, .. }
            | Self::FileInput { mute, .. }
            | Self::FolderReplay { mute, .. }
            | Self::ReplayInput { mute, .. }
            | Self::MemoryReplay { mute, .. }
            | Self::TransformPlugin { mute, .. }
            | Self::AudioProcessingGroup { mute, .. }
            | Self::Rvc { mute, .. }
            | Self::MultiInput { mute, .. }
            | Self::InputSelector { mute, .. }
            | Self::Delay { mute, .. }
            | Self::ChannelMapper { mute, .. }
            | Self::Monitor { mute, .. }
            | Self::VirtualDevice { mute, .. }
            | Self::Recorder { mute, .. }
            | Self::ReplayCache { mute, .. } => {
                *mute = value;
                true
            }
            Self::SilenceDetector { .. } | Self::Action { .. } | Self::LogOutput { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct NodeSpec {
    pub id: NodeId,
    /// Logical definition this graph instance was created from. `None` is
    /// retained for v0.1 graphs and ad-hoc unlimited instances.
    #[serde(default)]
    pub registered_node_id: Option<RegisteredNodeId>,
    pub node_type: NodeType,
    pub display_name: String,
    /// Legacy persisted flag. New code uses Registered Node `enabled` for
    /// availability and `runtime_enabled` for a placed graph instance.
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub runtime_enabled: bool,
    pub state: RuntimeState,
    #[serde(default)]
    pub comment: String,
    #[serde(default = "default_true")]
    pub show_in_playback: bool,
    #[serde(default)]
    pub playback_order: i32,
    pub input_ports: Vec<PortSpec>,
    pub output_ports: Vec<PortSpec>,
    pub properties: NodeProperties,
}

impl NodeSpec {
    pub fn new(
        node_type: NodeType,
        display_name: impl Into<String>,
        properties: NodeProperties,
    ) -> Self {
        let mut node = Self {
            id: NodeId::new(),
            registered_node_id: None,
            node_type,
            display_name: display_name.into(),
            enabled: true,
            runtime_enabled: true,
            state: RuntimeState::Stopped,
            comment: String::new(),
            show_in_playback: true,
            playback_order: 0,
            input_ports: Vec::new(),
            output_ports: Vec::new(),
            properties,
        };
        if matches!(node_type, NodeType::Action | NodeType::LogOutput) {
            node.input_ports
                .push(PortSpec::event_input("event in", Vec::new()));
            return node;
        }
        match node_type.class() {
            NodeClass::Input => node.output_ports.push(PortSpec::output("out")),
            NodeClass::Transform => {
                if node_type == NodeType::SilenceDetector {
                    node.input_ports.push(PortSpec::input("audio in"));
                    node.output_ports.push(PortSpec::event_output(
                        "event out",
                        vec![
                            "audio.silence.started".into(),
                            "audio.silence.resumed".into(),
                        ],
                    ));
                    return node;
                } else if node_type == NodeType::MultiInput {
                    let p = PortSpec::input("input 1");
                    if let NodeProperties::MultiInput { slots, .. } = &mut node.properties {
                        slots.push(MixSlot {
                            port_id: p.id,
                            edge_id: None,
                            gain_db: 0.0,
                            mute: false,
                        });
                    }
                    node.input_ports.push(p);
                } else if node_type == NodeType::InputSelector {
                    node.input_ports.push(PortSpec::input("input 1"));
                } else if node_type == NodeType::ChannelMapper {
                    node.input_ports.push(PortSpec::input("入力1 / L"));
                    let mut right = PortSpec::input("入力2 / R");
                    right.required = false;
                    node.input_ports.push(right);
                } else {
                    node.input_ports.push(PortSpec::input("in"));
                }
                node.output_ports.push(PortSpec::output("out"));
            }
            NodeClass::Output => node.input_ports.push(PortSpec::input("in")),
        }
        node
    }

    pub fn add_multi_input_port(&mut self, name: impl Into<String>) -> Option<PortId> {
        if self.node_type != NodeType::MultiInput {
            return None;
        }
        let p = PortSpec::input(name);
        let id = p.id;
        self.input_ports.push(p);
        if let NodeProperties::MultiInput { slots, .. } = &mut self.properties {
            slots.push(MixSlot {
                port_id: id,
                edge_id: None,
                gain_db: 0.0,
                mute: false,
            });
        }
        Some(id)
    }

    pub fn add_input_selector_port(&mut self, name: impl Into<String>) -> Option<PortId> {
        if self.node_type != NodeType::InputSelector {
            return None;
        }
        let port = PortSpec::input(name);
        let id = port.id;
        self.input_ports.push(port);
        Some(id)
    }

    pub fn has_port(&self, id: PortId, direction: PortDirection) -> bool {
        match direction {
            PortDirection::Input => self.input_ports.iter().any(|p| p.id == id),
            PortDirection::Output => self.output_ports.iter().any(|p| p.id == id),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct NodeLayout {
    pub node_id: NodeId,
    pub x: f32,
    pub y: f32,
}

impl NodeLayout {
    pub fn new(node_id: NodeId, x: f32, y: f32) -> Self {
        Self { node_id, x, y }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.x.is_finite() || !self.y.is_finite() {
            return Err("node layout coordinates must be finite".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct Edge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub from_port: PortId,
    pub to_node: NodeId,
    pub to_port: PortId,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub color: Option<String>,
}

impl Edge {
    pub fn new(from_node: NodeId, from_port: PortId, to_node: NodeId, to_port: PortId) -> Self {
        Self {
            id: EdgeId::new(),
            from_node,
            from_port,
            to_node,
            to_port,
            gain_db: 0.0,
            mute: false,
            color: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct EventEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub from_port: PortId,
    pub to_node: NodeId,
    pub to_port: PortId,
}

impl EventEdge {
    pub fn new(from_node: NodeId, from_port: PortId, to_node: NodeId, to_port: PortId) -> Self {
        Self {
            id: EdgeId::new(),
            from_node,
            from_port,
            to_node,
            to_port,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GraphSnapshot {
    pub revision: u64,
    pub nodes: HashMap<NodeId, NodeSpec>,
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub event_edges: Vec<EventEdge>,
    #[serde(default)]
    pub layouts: HashMap<NodeId, NodeLayout>,
}

impl Default for GraphSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            nodes: HashMap::new(),
            edges: Vec::new(),
            event_edges: Vec::new(),
            layouts: HashMap::new(),
        }
    }
}

impl GraphSnapshot {
    pub fn incoming<'a>(&'a self, node_id: NodeId) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.to_node == node_id)
    }
    pub fn outgoing<'a>(&'a self, node_id: NodeId) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.from_node == node_id)
    }
    pub fn incoming_events<'a>(
        &'a self,
        node_id: NodeId,
    ) -> impl Iterator<Item = &'a EventEdge> + 'a {
        self.event_edges
            .iter()
            .filter(move |edge| edge.to_node == node_id)
    }
    pub fn outgoing_events<'a>(
        &'a self,
        node_id: NodeId,
    ) -> impl Iterator<Item = &'a EventEdge> + 'a {
        self.event_edges
            .iter()
            .filter(move |edge| edge.from_node == node_id)
    }
}
