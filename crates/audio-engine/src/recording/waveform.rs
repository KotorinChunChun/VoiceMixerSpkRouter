use crate::{error::Result, recording::codecs::decode_audio_file};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const CACHE_TTL_SECONDS: u64 = 60 * 60 * 24 * 30;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct WaveformBucket {
    pub min: f32,
    pub max: f32,
    pub peak: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaveformCacheFile {
    source_size: u64,
    source_modified_unix_ms: u128,
    buckets: Vec<WaveformBucket>,
}

/// Returns display buckets for WAV/FLAC/MP3. The full file is decoded on the
/// calling worker side, never on the realtime audio thread.
pub fn get_waveform(path: impl AsRef<Path>, buckets: usize) -> Result<Vec<WaveformBucket>> {
    if buckets == 0 {
        return Ok(Vec::new());
    }
    let decoded = decode_audio_file(path)?;
    let samples = decoded.samples;
    if samples.is_empty() {
        return Ok(Vec::new());
    }
    let bucket_samples = (samples.len() + buckets - 1) / buckets;
    let mut out = Vec::with_capacity(buckets.min(samples.len()));
    for chunk in samples.chunks(bucket_samples) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut peak = 0.0f32;
        for &s in chunk {
            min = min.min(s);
            max = max.max(s);
            peak = peak.max(s.abs());
        }
        out.push(WaveformBucket { min, max, peak });
    }
    Ok(out)
}

/// Gets a waveform while reusing a JSON cache managed by the application.
/// Source size and modification time are part of the cache validation key, so
/// replacing a file at the same path never leaves a stale graph on screen.
pub fn get_cached_waveform(
    path: impl AsRef<Path>,
    buckets: usize,
    cache_dir: impl AsRef<Path>,
) -> Result<Vec<WaveformBucket>> {
    if buckets == 0 {
        return Ok(Vec::new());
    }
    let path = path.as_ref();
    let metadata = fs::metadata(path)?;
    let source_modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis())
        .unwrap_or_default();
    let cache_dir = cache_dir.as_ref();
    fs::create_dir_all(cache_dir)?;
    let cache_path = cache_path(path, buckets, cache_dir);

    cleanup_expired(cache_dir);
    if let Ok(bytes) = fs::read(&cache_path) {
        if let Ok(cached) = serde_json::from_slice::<WaveformCacheFile>(&bytes) {
            let cache_age = fs::metadata(&cache_path)
                .and_then(|value| value.modified())
                .ok()
                .and_then(|value| value.elapsed().ok())
                .map(|value| value.as_secs())
                .unwrap_or(CACHE_TTL_SECONDS + 1);
            if cached.source_size == metadata.len()
                && cached.source_modified_unix_ms == source_modified_unix_ms
                && !cached.buckets.is_empty()
                && cache_age <= CACHE_TTL_SECONDS
            {
                return Ok(cached.buckets);
            }
        }
    }

    let buckets = get_waveform(path, buckets)?;
    let payload = WaveformCacheFile {
        source_size: metadata.len(),
        source_modified_unix_ms,
        buckets: buckets.clone(),
    };
    let temporary = cache_path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(&payload)?)?;
    let _ = fs::remove_file(&cache_path);
    fs::rename(temporary, cache_path)?;
    Ok(buckets)
}

fn cache_path(path: &Path, buckets: usize, cache_dir: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    cache_dir.join(format!("{:016x}-{}.json", hasher.finish(), buckets))
}

fn cleanup_expired(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let expired = entry
            .metadata()
            .and_then(|value| value.modified())
            .ok()
            .and_then(|value| value.elapsed().ok())
            .map(|value| value.as_secs() > CACHE_TTL_SECONDS)
            .unwrap_or(false);
        if expired {
            let _ = fs::remove_file(path);
        }
    }
}
