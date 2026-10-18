use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::{
    audio::graph::{CacheId, RecordingFormat, ReplayItemId},
    error::{BackendError, Result},
    recording::codecs::decode_audio_file,
};

pub const DEFAULT_REPLAY_CACHE_MAX_ITEMS: usize = 50;
/// The display name used by older versions for a newly-created replay cache.
/// This is intentionally kept separate from the internal `replay_cache` kind.
pub const LEGACY_DEFAULT_REPLAY_CACHE_NAME: &str = "Replay Cache";
pub const DEFAULT_REPLAY_CACHE_NAME: &str = "メモリ録音";

/// Rename an unedited legacy default display name in any persisted replay
/// cache-owned object. Returns whether a migration was applied.
pub fn migrate_legacy_replay_cache_name(display_name: &mut String) -> bool {
    if display_name == LEGACY_DEFAULT_REPLAY_CACHE_NAME {
        *display_name = DEFAULT_REPLAY_CACHE_NAME.to_owned();
        true
    } else {
        false
    }
}

fn default_max_items() -> Option<usize> {
    Some(DEFAULT_REPLAY_CACHE_MAX_ITEMS)
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ReplayCacheConfig {
    pub cache_id: CacheId,
    pub display_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_record: bool,
    #[serde(default = "default_max_items")]
    pub max_items: Option<usize>,
}

impl ReplayCacheConfig {
    pub fn new(display_name: impl Into<String>) -> Self {
        Self {
            cache_id: CacheId::new(),
            display_name: display_name.into(),
            enabled: true,
            auto_record: false,
            max_items: default_max_items(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ReplayCacheItem {
    pub item_id: ReplayItemId,
    pub cache_id: CacheId,
    pub file_name: String,
    pub path: PathBuf,
    pub created_unix_ms: u64,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReplayCacheMetadata {
    #[serde(default)]
    pub caches: HashMap<CacheId, ReplayCacheConfig>,
    #[serde(default)]
    pub items: HashMap<ReplayItemId, ReplayCacheItem>,
}

pub struct ReplayCacheManager {
    base_dir: PathBuf,
    metadata: RwLock<ReplayCacheMetadata>,
}

impl ReplayCacheManager {
    pub fn new(base_dir: impl AsRef<Path>, metadata: ReplayCacheMetadata) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            metadata: RwLock::new(metadata),
        }
    }

    pub fn snapshot(&self) -> ReplayCacheMetadata {
        self.metadata.read().clone()
    }
    pub fn replace(&self, metadata: ReplayCacheMetadata) {
        *self.metadata.write() = migrate_default_display_names(metadata);
    }

    pub fn create_cache(&self, config: ReplayCacheConfig) -> Result<ReplayCacheConfig> {
        if matches!(config.max_items, Some(0)) {
            return Err(BackendError::InvalidOperation(
                "Replay Cache max_items must be greater than zero".into(),
            ));
        }
        let mut metadata = self.metadata.write();
        if metadata.caches.contains_key(&config.cache_id) {
            return Err(BackendError::InvalidOperation(format!(
                "Replay Cache {} already exists",
                config.cache_id
            )));
        }
        std::fs::create_dir_all(self.cache_dir(config.cache_id))?;
        metadata.caches.insert(config.cache_id, config.clone());
        Ok(config)
    }

    pub fn list_caches(&self) -> Vec<ReplayCacheConfig> {
        let mut caches: Vec<_> = self.metadata.read().caches.values().cloned().collect();
        caches.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        caches
    }

    pub fn get_cache(&self, cache_id: CacheId) -> Result<ReplayCacheConfig> {
        self.metadata
            .read()
            .caches
            .get(&cache_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::InvalidOperation(format!("Replay Cache {cache_id} not found"))
            })
    }

    pub fn update_cache(&self, config: ReplayCacheConfig) -> Result<ReplayCacheConfig> {
        if matches!(config.max_items, Some(0)) {
            return Err(BackendError::InvalidOperation(
                "Replay Cache max_items must be greater than zero".into(),
            ));
        }
        {
            let mut metadata = self.metadata.write();
            if !metadata.caches.contains_key(&config.cache_id) {
                return Err(BackendError::InvalidOperation(format!(
                    "Replay Cache {} not found",
                    config.cache_id
                )));
            }
            metadata.caches.insert(config.cache_id, config.clone());
        }
        self.enforce_limit(&config)?;
        Ok(config)
    }

    pub fn recording_staging_path(
        &self,
        cache_id: CacheId,
        format: RecordingFormat,
    ) -> Result<PathBuf> {
        self.get_cache(cache_id)?;
        let directory = self.base_dir.join("_recording");
        std::fs::create_dir_all(&directory)?;
        Ok(directory.join(format!("{}.{}", uuid::Uuid::new_v4(), format.extension())))
    }

    pub fn commit_recording(
        &self,
        cache_id: CacheId,
        staging_path: impl AsRef<Path>,
        created_unix_ms: u64,
    ) -> Result<ReplayCacheItem> {
        let staging_path = staging_path.as_ref();
        let item = self.add_file(cache_id, staging_path, created_unix_ms)?;
        if staging_path.exists() {
            std::fs::remove_file(staging_path)?;
        }
        Ok(item)
    }

    pub fn add_file(
        &self,
        cache_id: CacheId,
        source: impl AsRef<Path>,
        created_unix_ms: u64,
    ) -> Result<ReplayCacheItem> {
        let source = source.as_ref();
        let cache = self.get_cache(cache_id)?;
        let decoded = decode_audio_file(source)?;
        let item_id = ReplayItemId::new();
        let extension = source
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("wav");
        let file_name = format!("{}_{}.{}", created_unix_ms, item_id, extension);
        let destination = self.cache_dir(cache_id).join(&file_name);
        std::fs::create_dir_all(self.cache_dir(cache_id))?;
        std::fs::copy(source, &destination)?;
        let item = ReplayCacheItem {
            item_id,
            cache_id,
            file_name,
            path: destination,
            created_unix_ms,
            duration_seconds: decoded.samples.len() as f64 / (48_000.0 * 2.0),
        };
        self.metadata.write().items.insert(item_id, item.clone());
        self.enforce_limit(&cache)?;
        Ok(item)
    }

    pub fn list_items(&self, cache_id: CacheId) -> Result<Vec<ReplayCacheItem>> {
        self.get_cache(cache_id)?;
        let mut items: Vec<_> = self
            .metadata
            .read()
            .items
            .values()
            .filter(|item| item.cache_id == cache_id)
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            b.created_unix_ms
                .cmp(&a.created_unix_ms)
                .then_with(|| b.item_id.cmp(&a.item_id))
        });
        Ok(items)
    }

    pub fn latest(&self, cache_id: CacheId) -> Result<Option<ReplayCacheItem>> {
        Ok(self.list_items(cache_id)?.into_iter().next())
    }

    pub fn get_item(&self, cache_id: CacheId, item_id: ReplayItemId) -> Result<ReplayCacheItem> {
        self.metadata
            .read()
            .items
            .get(&item_id)
            .filter(|item| item.cache_id == cache_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::InvalidOperation(format!("Replay Cache item {item_id} not found"))
            })
    }

    fn enforce_limit(&self, cache: &ReplayCacheConfig) -> Result<()> {
        let Some(max_items) = cache.max_items else {
            return Ok(());
        };
        loop {
            let oldest = {
                let metadata = self.metadata.read();
                let count = metadata
                    .items
                    .values()
                    .filter(|item| item.cache_id == cache.cache_id)
                    .count();
                if count <= max_items {
                    return Ok(());
                }
                metadata
                    .items
                    .values()
                    .filter(|item| item.cache_id == cache.cache_id)
                    .min_by(|a, b| {
                        a.created_unix_ms
                            .cmp(&b.created_unix_ms)
                            .then_with(|| a.item_id.cmp(&b.item_id))
                    })
                    .cloned()
            };
            let Some(oldest) = oldest else {
                return Ok(());
            };
            self.metadata.write().items.remove(&oldest.item_id);
            if oldest.path.exists() {
                std::fs::remove_file(&oldest.path)?;
            }
        }
    }

    fn cache_dir(&self, cache_id: CacheId) -> PathBuf {
        self.base_dir.join(cache_id.to_string())
    }
}

/// Migrate only the exact name that the application used as its old default.
/// User-assigned names (including names such as `Replay Cache A`) are left as-is.
fn migrate_default_display_names(mut metadata: ReplayCacheMetadata) -> ReplayCacheMetadata {
    for cache in metadata.caches.values_mut() {
        migrate_legacy_replay_cache_name(&mut cache.display_name);
    }
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_only_the_unedited_legacy_default_name() {
        let legacy_id = CacheId::new();
        let custom_id = CacheId::new();
        let similarly_named_id = CacheId::new();
        let mut metadata = ReplayCacheMetadata::default();
        metadata.caches.insert(
            legacy_id,
            ReplayCacheConfig {
                cache_id: legacy_id,
                display_name: LEGACY_DEFAULT_REPLAY_CACHE_NAME.into(),
                ..ReplayCacheConfig::new(LEGACY_DEFAULT_REPLAY_CACHE_NAME)
            },
        );
        metadata.caches.insert(
            custom_id,
            ReplayCacheConfig {
                cache_id: custom_id,
                display_name: "会議用 Replay Cache".into(),
                ..ReplayCacheConfig::new("会議用 Replay Cache")
            },
        );
        metadata.caches.insert(
            similarly_named_id,
            ReplayCacheConfig {
                cache_id: similarly_named_id,
                display_name: "Replay Cache A".into(),
                ..ReplayCacheConfig::new("Replay Cache A")
            },
        );

        let manager = ReplayCacheManager::new(std::env::temp_dir(), metadata);
        manager.replace(manager.snapshot());
        let migrated = manager.snapshot();
        assert_eq!(
            migrated.caches[&legacy_id].display_name,
            DEFAULT_REPLAY_CACHE_NAME
        );
        assert_eq!(
            migrated.caches[&custom_id].display_name,
            "会議用 Replay Cache"
        );
        assert_eq!(
            migrated.caches[&similarly_named_id].display_name,
            "Replay Cache A"
        );
        assert_eq!(migrated.caches[&legacy_id].cache_id, legacy_id);
    }
}
