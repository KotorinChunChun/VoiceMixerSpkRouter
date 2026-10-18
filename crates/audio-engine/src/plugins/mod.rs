pub mod builtins;
pub mod instance;
pub mod manager;
pub mod manifest;
pub mod package;
pub mod process_host;
pub mod process_protocol;
pub mod shared_audio;
pub mod voice_models;
pub mod voice_package;

use std::sync::Arc;

use crate::{audio::graph::NodeProperties, error::Result};
use manager::PluginManager;

pub const RVC_CUDA_PLUGIN_ID: &str = "voicemixer.rvc.cuda";
pub const LEGACY_RVC_KIKI_PLUGIN_ID: &str = "voicemixer.rvc.kiki-v1";
const LEGACY_KIKI_MODEL: &str = "models/kikiV1.pth";
const LEGACY_KIKI_INDEX: &str = "models/kikiV1.index";
const DEFAULT_KAHO_MODEL: &str = "models/KahoA.pth";
const DEFAULT_KAHO_INDEX: &str = "models/Kaho.index";

/// Rewrites the former model-specific ID while loading persisted settings.
pub fn migrate_legacy_rvc_plugin_id(properties: &mut NodeProperties) -> bool {
    if let NodeProperties::AudioProcessingGroup { flow, .. } = properties {
        let mut migrated = false;
        for member in flow {
            if member.plugin_id == LEGACY_RVC_KIKI_PLUGIN_ID {
                member.plugin_id = RVC_CUDA_PLUGIN_ID.into();
                if member
                    .parameters
                    .get("model")
                    .and_then(|value| value.as_str())
                    == Some(LEGACY_KIKI_MODEL)
                {
                    member
                        .parameters
                        .insert("model".into(), DEFAULT_KAHO_MODEL.into());
                }
                if member
                    .parameters
                    .get("index")
                    .and_then(|value| value.as_str())
                    == Some(LEGACY_KIKI_INDEX)
                {
                    member
                        .parameters
                        .insert("index".into(), DEFAULT_KAHO_INDEX.into());
                }
                migrated = true;
            }
        }
        return migrated;
    }
    let NodeProperties::TransformPlugin {
        plugin_id,
        parameters,
        ..
    } = properties
    else {
        return false;
    };
    if plugin_id != LEGACY_RVC_KIKI_PLUGIN_ID {
        return false;
    }
    *plugin_id = RVC_CUDA_PLUGIN_ID.into();
    if parameters.get("model").and_then(|value| value.as_str()) == Some(LEGACY_KIKI_MODEL) {
        parameters.insert("model".into(), DEFAULT_KAHO_MODEL.into());
    }
    if parameters.get("index").and_then(|value| value.as_str()) == Some(LEGACY_KIKI_INDEX) {
        parameters.insert("index".into(), DEFAULT_KAHO_INDEX.into());
    }
    true
}

pub fn create_default_plugin_manager() -> Result<Arc<PluginManager>> {
    let manager = Arc::new(PluginManager::default());
    manager.register_factory(builtins::limiter::LimiterFactory::shared())?;
    manager.register_factory(builtins::equalizer::EqualizerFactory::shared())?;
    manager.register_factory(builtins::gate::GateFactory::shared())?;
    manager.register_factory(builtins::compressor::CompressorFactory::shared())?;
    manager.register_factory(builtins::noise_suppressor::NoiseSuppressorFactory::shared())?;
    manager.register_factory(builtins::ducking::DuckingFactory::shared())?;
    Ok(manager)
}
