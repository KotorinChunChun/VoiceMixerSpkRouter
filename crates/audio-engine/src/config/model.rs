use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    actions::model::{ObsConnectionConfig, ObsConnectionId},
    audio::graph::{
        CaptureMethod, Edge, EventEdge, NodeId, NodeLayout, NodeSpec, RegisteredNodeId,
    },
    profile::volume_profile::{VolumeMuteProfile, VolumeProfileId},
    registry::{device_history::DeviceHistory, node_registry::RegisteredNode},
    replay::cache::ReplayCacheMetadata,
    shortcut::model::{ShortcutBinding, ShortcutId},
};

pub const DEFAULT_BLOCK_FRAMES: usize = 480;
pub const DEFAULT_METER_FPS: u32 = 30;
pub const DEFAULT_FONT_FAMILY: &str = "system";
pub const DEFAULT_WAVEFORM_GRAPH_MAX_DBFS: f32 = -12.0;
pub const DEFAULT_LEVEL_METER_MAX_DBFS: f32 = -6.0;
pub const DEFAULT_ROUTING_EDGE_NEAR_DISTANCE_PX: f32 = 30.0;
pub const DEFAULT_ROUTING_UNRELATED_NODE_OPACITY: f32 = 0.60;
pub const JITTER_FILL_LOW: f32 = 0.25;
pub const JITTER_FILL_BALANCED: f32 = 0.5;
pub const JITTER_FILL_STABLE: f32 = 0.75;
pub const LOW_LATENCY_BLOCK_FRAMES: usize = 128;
pub const STABLE_BLOCK_FRAMES: usize = 960;

const SUPPORTED_FONT_FAMILIES: &[&str] = &[
    "system",
    "yu_gothic",
    "meiryo",
    "biz_udpgothic",
    "sans_serif",
];

fn default_plugin_search_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("plugins")]
}

fn default_font_family() -> String {
    DEFAULT_FONT_FAMILY.into()
}

fn default_waveform_graph_max_dbfs() -> f32 {
    DEFAULT_WAVEFORM_GRAPH_MAX_DBFS
}

fn default_level_meter_max_dbfs() -> f32 {
    DEFAULT_LEVEL_METER_MAX_DBFS
}

fn default_routing_edge_near_distance_px() -> f32 {
    DEFAULT_ROUTING_EDGE_NEAR_DISTANCE_PX
}

fn default_routing_unrelated_node_opacity() -> f32 {
    DEFAULT_ROUTING_UNRELATED_NODE_OPACITY
}

pub fn is_supported_font_family(value: &str) -> bool {
    SUPPORTED_FONT_FAMILIES.contains(&value)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DevicePreference {
    pub visible: bool,
    pub sort_order: i32,
}

impl Default for DevicePreference {
    fn default() -> Self {
        Self {
            visible: true,
            sort_order: i32::MAX,
        }
    }
}

/// Defaults used when the frontend creates the built-in limiter transform.
/// `enabled` is false by default: merely having these defaults never inserts
/// or activates a limiter in an existing route.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LimiterDefaults {
    pub enabled: bool,
    pub ceiling_db: f32,
    pub lookahead_ms: f32,
    pub release_ms: f32,
}

impl Default for LimiterDefaults {
    fn default() -> Self {
        Self {
            enabled: false,
            ceiling_db: -1.0,
            lookahead_ms: 3.0,
            release_ms: 100.0,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LatencyPreset {
    LowLatency,
    #[default]
    Balanced,
    Stable,
    Custom,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RoutingEdgeDrawMode {
    #[default]
    Bezier,
    Rounded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PresetValues {
    pub block_frames: usize,
    pub input_method_priority: Vec<CaptureMethod>,
    pub render_exclusive: bool,
    pub jitter_target_fill: f32,
}

fn missing_latency_preset() -> LatencyPreset {
    LatencyPreset::Custom
}

fn default_jitter_target_fill() -> f32 {
    JITTER_FILL_BALANCED
}

pub const DEFAULT_EDGE_PORT: u16 = 38_471;

fn default_edge_port() -> u16 {
    DEFAULT_EDGE_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplicationSettings {
    /// Realtime graph block size. 480 frames = 10 ms at 48 kHz.
    pub block_frames: usize,
    /// Aggregated level event rate. This does not affect audio processing.
    pub meter_fps: u32,
    pub auto_reconnect_devices: bool,
    /// CSS is selected from a fixed frontend allowlist so arbitrary values
    /// cannot be persisted as UI styles.
    #[serde(default = "default_font_family")]
    pub font_family: String,
    /// Upper display limit for cached waveform graphs. This only changes the
    /// visualization scale and never alters recorded or replayed audio.
    #[serde(default = "default_waveform_graph_max_dbfs")]
    pub waveform_graph_max_dbfs: f32,
    /// Upper display limit for live input level meters. This only changes the
    /// visualization scale and never alters recorded or replayed audio.
    #[serde(default = "default_level_meter_max_dbfs")]
    pub level_meter_max_dbfs: f32,
    /// Visual routing edge style. This does not alter graph connectivity.
    #[serde(default)]
    pub routing_edge_draw_mode: RoutingEdgeDrawMode,
    /// Horizontal node gap at or below which an overlap-safe route is used.
    #[serde(default = "default_routing_edge_near_distance_px")]
    pub routing_edge_near_distance_px: f32,
    /// Opacity applied to nodes unrelated to the current selection.
    #[serde(default = "default_routing_unrelated_node_opacity")]
    pub routing_unrelated_node_opacity: f32,
    /// Search roots for external transform plugins. Relative paths are resolved
    /// against the directory containing the backend configuration file.
    #[serde(default = "default_plugin_search_paths")]
    pub plugin_search_paths: Vec<PathBuf>,
    #[serde(default)]
    pub limiter_defaults: LimiterDefaults,
    #[serde(default)]
    pub device_preferences: HashMap<String, DevicePreference>,
    /// Missing field in old files is Custom. New `Default` uses Balanced.
    #[serde(default = "missing_latency_preset")]
    pub latency_preset: LatencyPreset,
    #[serde(default)]
    pub render_exclusive: bool,
    /// Edge PCM拡張との接続認証に使うトークン。旧設定では起動時に生成して保存する。
    #[serde(default)]
    pub edge_auth_token: String,
    /// Edge PCM拡張のloopback待受ポート。変更は次回アプリ起動時に反映する。
    #[serde(default = "default_edge_port")]
    pub edge_port: u16,
    #[serde(default = "default_jitter_target_fill")]
    pub jitter_target_fill: f32,
    #[serde(default)]
    pub auto_capture_priority: Vec<CaptureMethod>,
}

pub fn expand_preset(preset: LatencyPreset, current: &ApplicationSettings) -> PresetValues {
    match preset {
        LatencyPreset::LowLatency => PresetValues {
            block_frames: LOW_LATENCY_BLOCK_FRAMES,
            input_method_priority: vec![
                CaptureMethod::WasapiExclusiveEvent,
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ],
            render_exclusive: true,
            jitter_target_fill: JITTER_FILL_LOW,
        },
        LatencyPreset::Balanced => PresetValues {
            block_frames: DEFAULT_BLOCK_FRAMES,
            input_method_priority: vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ],
            render_exclusive: false,
            jitter_target_fill: JITTER_FILL_BALANCED,
        },
        LatencyPreset::Stable => PresetValues {
            block_frames: STABLE_BLOCK_FRAMES,
            input_method_priority: vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ],
            render_exclusive: false,
            jitter_target_fill: JITTER_FILL_STABLE,
        },
        LatencyPreset::Custom => PresetValues {
            block_frames: current.block_frames,
            input_method_priority: current.auto_capture_priority.clone(),
            render_exclusive: current.render_exclusive,
            jitter_target_fill: current.jitter_target_fill,
        },
    }
}

impl ApplicationSettings {
    pub fn apply_preset_values(&mut self, preset: LatencyPreset, values: &PresetValues) {
        self.latency_preset = preset;
        self.block_frames = values.block_frames;
        self.auto_capture_priority = values.input_method_priority.clone();
        self.render_exclusive = values.render_exclusive;
        self.jitter_target_fill = values.jitter_target_fill;
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(64..=4096).contains(&self.block_frames) {
            return Err("block_frames must be between 64 and 4096".into());
        }
        if !(1..=120).contains(&self.meter_fps) {
            return Err("meter_fps must be between 1 and 120".into());
        }
        if !is_supported_font_family(&self.font_family) {
            return Err(format!(
                "font_family must be one of: {}",
                SUPPORTED_FONT_FAMILIES.join(", ")
            ));
        }
        if !self.waveform_graph_max_dbfs.is_finite()
            || !(-60.0..=0.0).contains(&self.waveform_graph_max_dbfs)
        {
            return Err("waveform_graph_max_dbfs must be between -60 and 0 dBFS".into());
        }
        if !self.level_meter_max_dbfs.is_finite()
            || !(-60.0..=0.0).contains(&self.level_meter_max_dbfs)
        {
            return Err("level_meter_max_dbfs must be between -60 and 0 dBFS".into());
        }
        if !self.routing_edge_near_distance_px.is_finite()
            || !(0.0..=160.0).contains(&self.routing_edge_near_distance_px)
        {
            return Err("routing_edge_near_distance_px must be between 0 and 160 px".into());
        }
        if !self.routing_unrelated_node_opacity.is_finite()
            || !(0.0..=1.0).contains(&self.routing_unrelated_node_opacity)
        {
            return Err("routing_unrelated_node_opacity must be between 0 and 1".into());
        }
        let limiter = &self.limiter_defaults;
        if !limiter.ceiling_db.is_finite() || !(-24.0..=0.0).contains(&limiter.ceiling_db) {
            return Err("limiter ceiling_db must be between -24 and 0 dBFS".into());
        }
        if !limiter.lookahead_ms.is_finite() || !(0.0..=20.0).contains(&limiter.lookahead_ms) {
            return Err("limiter lookahead_ms must be between 0 and 20 ms".into());
        }
        if !limiter.release_ms.is_finite() || !(5.0..=5000.0).contains(&limiter.release_ms) {
            return Err("limiter release_ms must be between 5 and 5000 ms".into());
        }
        if !self.jitter_target_fill.is_finite() || !(0.05..=0.95).contains(&self.jitter_target_fill)
        {
            return Err("jitter_target_fill must be between 0.05 and 0.95".into());
        }
        if self.edge_port == 0 {
            return Err("edge_port must be between 1 and 65535".into());
        }
        Ok(())
    }
}

impl Default for ApplicationSettings {
    fn default() -> Self {
        Self {
            block_frames: DEFAULT_BLOCK_FRAMES,
            meter_fps: DEFAULT_METER_FPS,
            auto_reconnect_devices: true,
            font_family: default_font_family(),
            waveform_graph_max_dbfs: default_waveform_graph_max_dbfs(),
            level_meter_max_dbfs: default_level_meter_max_dbfs(),
            routing_edge_draw_mode: RoutingEdgeDrawMode::default(),
            routing_edge_near_distance_px: default_routing_edge_near_distance_px(),
            routing_unrelated_node_opacity: default_routing_unrelated_node_opacity(),
            plugin_search_paths: default_plugin_search_paths(),
            limiter_defaults: LimiterDefaults::default(),
            device_preferences: HashMap::new(),
            latency_preset: LatencyPreset::Balanced,
            render_exclusive: false,
            edge_auth_token: String::new(),
            edge_port: DEFAULT_EDGE_PORT,
            jitter_target_fill: JITTER_FILL_BALANCED,
            auto_capture_priority: vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        expand_preset, ApplicationSettings, LatencyPreset, PersistedConfiguration,
        RoutingEdgeDrawMode, DEFAULT_BLOCK_FRAMES, DEFAULT_FONT_FAMILY,
        DEFAULT_LEVEL_METER_MAX_DBFS, DEFAULT_ROUTING_EDGE_NEAR_DISTANCE_PX,
        DEFAULT_ROUTING_UNRELATED_NODE_OPACITY, DEFAULT_WAVEFORM_GRAPH_MAX_DBFS,
        JITTER_FILL_BALANCED, JITTER_FILL_LOW, JITTER_FILL_STABLE, LOW_LATENCY_BLOCK_FRAMES,
        STABLE_BLOCK_FRAMES,
    };
    use crate::audio::graph::CaptureMethod;
    use crate::config::persistence::ConfigStore;

    #[test]
    fn application_settings_default_uses_readable_system_font() {
        let settings = ApplicationSettings::default();
        assert_eq!(settings.edge_port, super::DEFAULT_EDGE_PORT);
        assert_eq!(settings.font_family, DEFAULT_FONT_FAMILY);
        assert_eq!(
            settings.waveform_graph_max_dbfs,
            DEFAULT_WAVEFORM_GRAPH_MAX_DBFS
        );
        assert_eq!(settings.level_meter_max_dbfs, DEFAULT_LEVEL_METER_MAX_DBFS);
        assert_eq!(settings.routing_edge_draw_mode, RoutingEdgeDrawMode::Bezier);
        assert_eq!(
            settings.routing_edge_near_distance_px,
            DEFAULT_ROUTING_EDGE_NEAR_DISTANCE_PX
        );
        assert_eq!(
            settings.routing_unrelated_node_opacity,
            DEFAULT_ROUTING_UNRELATED_NODE_OPACITY
        );
        settings.validate().unwrap();
    }

    #[test]
    fn application_settings_rejects_unknown_font_family() {
        let mut settings = ApplicationSettings::default();
        settings.font_family = "untrusted-css".into();
        let error = settings.validate().unwrap_err();
        assert!(error.contains("font_family"));
    }

    #[test]
    fn application_settings_rejects_zero_edge_port_and_migrates_missing_port() {
        let mut settings = ApplicationSettings::default();
        settings.edge_port = 0;
        assert!(settings.validate().unwrap_err().contains("edge_port"));

        let mut legacy = serde_json::to_value(ApplicationSettings::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("edge_port");
        let restored: ApplicationSettings = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.edge_port, super::DEFAULT_EDGE_PORT);
    }

    #[test]
    fn application_settings_rejects_waveform_graph_limit_outside_dbfs_range() {
        let mut settings = ApplicationSettings::default();
        settings.waveform_graph_max_dbfs = 0.1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("waveform_graph_max_dbfs"));

        settings.waveform_graph_max_dbfs = -60.1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("waveform_graph_max_dbfs"));
    }

    #[test]
    fn application_settings_rejects_level_meter_limit_outside_dbfs_range() {
        let mut settings = ApplicationSettings::default();
        settings.level_meter_max_dbfs = 0.1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("level_meter_max_dbfs"));

        settings.level_meter_max_dbfs = -60.1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("level_meter_max_dbfs"));
    }

    #[test]
    fn application_settings_validate_routing_display_ranges() {
        let mut settings = ApplicationSettings::default();
        settings.routing_edge_near_distance_px = 160.1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("routing_edge_near_distance_px"));

        settings.routing_edge_near_distance_px = 30.0;
        settings.routing_unrelated_node_opacity = -0.01;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("routing_unrelated_node_opacity"));

        settings.routing_unrelated_node_opacity = 1.0;
        settings.validate().unwrap();
    }

    #[test]
    fn routing_display_settings_are_backward_compatible_and_round_trip() {
        let mut legacy = serde_json::to_value(ApplicationSettings::default()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("routing_edge_draw_mode");
        object.remove("routing_edge_near_distance_px");
        object.remove("routing_unrelated_node_opacity");
        let restored: ApplicationSettings = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.routing_edge_draw_mode, RoutingEdgeDrawMode::Bezier);
        assert_eq!(restored.routing_edge_near_distance_px, 30.0);
        assert_eq!(restored.routing_unrelated_node_opacity, 0.60);

        let changed = ApplicationSettings {
            routing_edge_draw_mode: RoutingEdgeDrawMode::Rounded,
            routing_edge_near_distance_px: 72.0,
            routing_unrelated_node_opacity: 0.35,
            ..ApplicationSettings::default()
        };
        let round_trip: ApplicationSettings =
            serde_json::from_value(serde_json::to_value(&changed).unwrap()).unwrap();
        assert_eq!(
            round_trip.routing_edge_draw_mode,
            RoutingEdgeDrawMode::Rounded
        );
        assert_eq!(round_trip.routing_edge_near_distance_px, 72.0);
        assert_eq!(round_trip.routing_unrelated_node_opacity, 0.35);
    }

    #[test]
    fn t02_u_01_expand_preset_matches_table() {
        let current = ApplicationSettings::default();
        let low = expand_preset(LatencyPreset::LowLatency, &current);
        assert_eq!(low.block_frames, LOW_LATENCY_BLOCK_FRAMES);
        assert_eq!(
            low.input_method_priority,
            vec![
                CaptureMethod::WasapiExclusiveEvent,
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ]
        );
        assert!(low.render_exclusive);
        assert_eq!(low.jitter_target_fill, JITTER_FILL_LOW);

        let balanced = expand_preset(LatencyPreset::Balanced, &current);
        assert_eq!(balanced.block_frames, DEFAULT_BLOCK_FRAMES);
        assert_eq!(
            balanced.input_method_priority,
            vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ]
        );
        assert!(!balanced.render_exclusive);
        assert_eq!(balanced.jitter_target_fill, JITTER_FILL_BALANCED);

        let stable = expand_preset(LatencyPreset::Stable, &current);
        assert_eq!(stable.block_frames, STABLE_BLOCK_FRAMES);
        assert_eq!(
            stable.input_method_priority,
            vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ]
        );
        assert!(!stable.render_exclusive);
        assert_eq!(stable.jitter_target_fill, JITTER_FILL_STABLE);
    }

    #[test]
    fn t02_u_02_expand_custom_returns_current() {
        let mut current = ApplicationSettings::default();
        current.block_frames = 256;
        current.render_exclusive = true;
        current.jitter_target_fill = 0.4;
        current.auto_capture_priority = vec![CaptureMethod::WasapiSharedPolling];
        let values = expand_preset(LatencyPreset::Custom, &current);
        assert_eq!(values.block_frames, 256);
        assert!(values.render_exclusive);
        assert_eq!(values.jitter_target_fill, 0.4);
        assert_eq!(
            values.input_method_priority,
            vec![CaptureMethod::WasapiSharedPolling]
        );
    }

    #[test]
    fn t02_u_03_legacy_settings_json_loads_as_custom() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("voicemixer.json");
        let json = r#"{
            "nodes": {},
            "routing_edges": [],
            "settings": {
                "block_frames": 512,
                "meter_fps": 24,
                "auto_reconnect_devices": false,
                "font_family": "meiryo"
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let loaded = ConfigStore::new(&path).load().unwrap();
        assert_eq!(loaded.settings.latency_preset, LatencyPreset::Custom);
        assert_eq!(loaded.settings.block_frames, 512);
        assert_eq!(loaded.settings.meter_fps, 24);
        assert!(!loaded.settings.auto_reconnect_devices);
        assert_eq!(loaded.settings.font_family, "meiryo");
        assert_eq!(
            loaded.settings.waveform_graph_max_dbfs,
            DEFAULT_WAVEFORM_GRAPH_MAX_DBFS
        );
        assert_eq!(
            loaded.settings.level_meter_max_dbfs,
            DEFAULT_LEVEL_METER_MAX_DBFS
        );
        loaded.settings.validate().unwrap();
    }

    #[test]
    fn t02_u_04_new_settings_default_to_balanced() {
        let settings = ApplicationSettings::default();
        assert_eq!(settings.latency_preset, LatencyPreset::Balanced);
        assert_eq!(settings.block_frames, DEFAULT_BLOCK_FRAMES);
        settings.validate().unwrap();
        let persisted = PersistedConfiguration::default();
        assert_eq!(persisted.settings.latency_preset, LatencyPreset::Balanced);
    }

    proptest::proptest! {
        #[test]
        fn t02_p_01_settings_round_trip(
            block in 64usize..=4096,
            meter in 1u32..=120,
            exclusive: bool,
            fill in 0.05f32..=0.95,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let store = ConfigStore::new(temp.path().join("voicemixer.json"));
            let mut config = PersistedConfiguration::default();
            config.settings.block_frames = block;
            config.settings.meter_fps = meter;
            config.settings.latency_preset = LatencyPreset::Custom;
            config.settings.render_exclusive = exclusive;
            config.settings.jitter_target_fill = fill;
            config.settings.auto_capture_priority = vec![CaptureMethod::WinMmWaveIn];
            store.save(&config).unwrap();
            let loaded = store.load().unwrap();
            assert_eq!(loaded.settings.block_frames, block);
            assert_eq!(loaded.settings.meter_fps, meter);
            assert_eq!(loaded.settings.latency_preset, LatencyPreset::Custom);
            assert_eq!(loaded.settings.render_exclusive, exclusive);
            assert!((loaded.settings.jitter_target_fill - fill).abs() < 1e-5);
            assert_eq!(
                loaded.settings.auto_capture_priority,
                vec![CaptureMethod::WinMmWaveIn]
            );
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedConfiguration {
    #[serde(default)]
    pub registered_nodes: HashMap<RegisteredNodeId, RegisteredNode>,
    pub nodes: HashMap<NodeId, NodeSpec>,
    pub routing_edges: Vec<Edge>,
    #[serde(default)]
    pub event_edges: Vec<EventEdge>,
    #[serde(default)]
    pub obs_connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
    #[serde(default)]
    pub shortcuts: HashMap<ShortcutId, ShortcutBinding>,
    #[serde(default)]
    pub node_layouts: HashMap<NodeId, NodeLayout>,
    #[serde(default)]
    pub device_history: DeviceHistory,
    #[serde(default)]
    pub replay_cache_metadata: ReplayCacheMetadata,
    #[serde(default)]
    pub volume_profiles: HashMap<VolumeProfileId, VolumeMuteProfile>,
    #[serde(default)]
    pub settings: ApplicationSettings,
    /// Virtual render/capture endpoint relationships are supplied by the
    /// frontend. The backend never guesses vendor-specific pairings.
    #[serde(default)]
    pub virtual_device_pairs: Vec<crate::windows::implicit_routes::VirtualDevicePair>,
    /// The last routing profile selected in the frontend, if any.
    #[serde(default)]
    pub active_routing_profile: Option<String>,
    /// Bounded by the placement command; enables safe retry after a lost UI response.
    #[serde(default)]
    pub completed_playback_device_operations: HashMap<uuid::Uuid, NodeId>,
}
