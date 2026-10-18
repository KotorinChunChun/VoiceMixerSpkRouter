use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    audio::{
        engine::AudioSource,
        format::{AudioBlock, INTERNAL_SAMPLE_RATE},
    },
    error::{BackendError, Result},
    recording::codecs::decode_audio_file,
    replay::state::{ReplayPlaybackState, ReplayStateSnapshot},
};

pub trait ReplaySource: Send + Sync {
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn seek(&self, position: Duration) -> Result<()>;
    fn position(&self) -> Duration;
    fn duration(&self) -> Option<Duration>;
    fn snapshot(&self) -> ReplayStateSnapshot;
}

pub struct MemoryReplayPlayer {
    samples: Arc<Vec<f32>>,
    cursor: AtomicU64,
    state: AtomicU8,
    eof: AtomicBool,
    loop_enabled: bool,
    loop_wait_samples: u64,
    loop_wait_remaining: AtomicU64,
    source_path: Option<PathBuf>,
}

impl MemoryReplayPlayer {
    pub fn open(path: impl AsRef<Path>, loop_enabled: bool) -> Result<Self> {
        Self::open_with_wait(path, loop_enabled, 0)
    }

    pub fn open_with_wait(
        path: impl AsRef<Path>,
        loop_enabled: bool,
        loop_wait_ms: u64,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let block = decode_audio_file(&path)?;
        Ok(Self::new_with_path(
            block.samples,
            loop_enabled,
            loop_wait_ms,
            Some(path),
        ))
    }

    pub fn new(samples: Vec<f32>, loop_enabled: bool) -> Self {
        Self::new_with_wait(samples, loop_enabled, 0)
    }

    pub fn new_with_wait(samples: Vec<f32>, loop_enabled: bool, loop_wait_ms: u64) -> Self {
        Self::new_with_path(samples, loop_enabled, loop_wait_ms, None)
    }

    fn new_with_path(
        samples: Vec<f32>,
        loop_enabled: bool,
        loop_wait_ms: u64,
        source_path: Option<PathBuf>,
    ) -> Self {
        let loop_wait_samples = duration_to_sample(Duration::from_millis(loop_wait_ms));
        Self {
            samples: Arc::new(samples),
            cursor: AtomicU64::new(0),
            state: AtomicU8::new(ReplayPlaybackState::Stopped.as_u8()),
            eof: AtomicBool::new(false),
            loop_enabled,
            loop_wait_samples,
            loop_wait_remaining: AtomicU64::new(0),
            source_path,
        }
    }

    fn playback_state(&self) -> ReplayPlaybackState {
        ReplayPlaybackState::from_u8(self.state.load(Ordering::Acquire))
    }
}

impl ReplaySource for MemoryReplayPlayer {
    fn play(&self) -> Result<()> {
        if self.samples.is_empty() {
            return Err(BackendError::InvalidOperation(
                "replay contains no audio".into(),
            ));
        }
        if self.cursor.load(Ordering::Acquire) as usize >= self.samples.len() {
            self.cursor.store(0, Ordering::Release);
        }
        self.loop_wait_remaining.store(0, Ordering::Release);
        self.eof.store(false, Ordering::Release);
        self.state
            .store(ReplayPlaybackState::Playing.as_u8(), Ordering::Release);
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        if self.playback_state() == ReplayPlaybackState::Playing {
            self.state
                .store(ReplayPlaybackState::Paused.as_u8(), Ordering::Release);
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.state
            .store(ReplayPlaybackState::Stopped.as_u8(), Ordering::Release);
        self.cursor.store(0, Ordering::Release);
        self.eof.store(false, Ordering::Release);
        self.loop_wait_remaining.store(0, Ordering::Release);
        Ok(())
    }

    fn seek(&self, position: Duration) -> Result<()> {
        let sample = duration_to_sample(position).min(self.samples.len() as u64);
        self.cursor.store(sample, Ordering::Release);
        self.eof.store(false, Ordering::Release);
        self.loop_wait_remaining.store(0, Ordering::Release);
        Ok(())
    }

    fn position(&self) -> Duration {
        sample_to_duration(self.cursor.load(Ordering::Acquire))
    }

    fn duration(&self) -> Option<Duration> {
        Some(sample_to_duration(self.samples.len() as u64))
    }

    fn snapshot(&self) -> ReplayStateSnapshot {
        ReplayStateSnapshot {
            state: self.playback_state(),
            position_seconds: self.position().as_secs_f64(),
            duration_seconds: self.duration().map(|duration| duration.as_secs_f64()),
            eof: self.eof.load(Ordering::Acquire),
            current_file: self
                .source_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
        }
    }
}

impl AudioSource for MemoryReplayPlayer {
    fn replay_snapshot(&self) -> Option<ReplayStateSnapshot> {
        Some(self.snapshot())
    }

    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        if self.playback_state() != ReplayPlaybackState::Playing || self.samples.is_empty() {
            return Ok(());
        }
        let mut cursor = self.cursor.load(Ordering::Acquire) as usize;
        let mut wait_remaining = self.loop_wait_remaining.load(Ordering::Acquire);
        let mut written = 0usize;
        while written < block.samples.len() {
            if wait_remaining > 0 {
                let count = (block.samples.len() - written).min(wait_remaining as usize);
                // AudioBlock was cleared above, so this range is the required
                // silence between loop iterations.
                written += count;
                wait_remaining -= count as u64;
                continue;
            }
            if cursor >= self.samples.len() {
                if self.loop_enabled {
                    if self.loop_wait_samples > 0 {
                        wait_remaining = self.loop_wait_samples;
                        cursor = 0;
                        continue;
                    }
                    cursor = 0;
                } else {
                    self.state
                        .store(ReplayPlaybackState::Stopped.as_u8(), Ordering::Release);
                    self.eof.store(true, Ordering::Release);
                    break;
                }
            }
            let count = (block.samples.len() - written).min(self.samples.len() - cursor);
            block.samples[written..written + count]
                .copy_from_slice(&self.samples[cursor..cursor + count]);
            cursor += count;
            written += count;
        }
        self.cursor.store(cursor as u64, Ordering::Release);
        self.loop_wait_remaining
            .store(wait_remaining, Ordering::Release);
        Ok(())
    }
}

pub(crate) fn duration_to_sample(duration: Duration) -> u64 {
    (duration.as_secs_f64() * INTERNAL_SAMPLE_RATE as f64 * 2.0).round() as u64
}

pub(crate) fn sample_to_duration(sample: u64) -> Duration {
    Duration::from_secs_f64(sample as f64 / (INTERNAL_SAMPLE_RATE as f64 * 2.0))
}
