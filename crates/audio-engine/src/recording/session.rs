use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;
use parking_lot::RwLock;

use crate::{
    audio::{
        engine::AudioSink,
        format::{AudioBlock, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        graph::{CacheId, RecordingFormat},
    },
    error::Result,
    recording::{
        state::{RecorderState, RecorderStateSnapshot, RecordingOutput},
        writer::RecorderSink,
    },
    replay::cache::ReplayCacheManager,
};

/// Recordings shorter than this are treated as accidental start/stop actions.
/// The same threshold is used by the UI and by the backend so a discarded take
/// can never be committed to a replay cache.
pub const MIN_RECORDED_SECONDS: f64 = 3.0;

enum RecordingDestination {
    Folder {
        directory: PathBuf,
        format: RecordingFormat,
    },
    FixedFile {
        path: PathBuf,
        format: RecordingFormat,
    },
    ReplayCache {
        cache_id: CacheId,
        format: RecordingFormat,
        manager: Arc<ReplayCacheManager>,
    },
}

pub struct ManagedRecorder {
    destination: RecordingDestination,
    writer: ArcSwapOption<RecorderSink>,
    state: AtomicU8,
    auto_record: AtomicBool,
    session_active: AtomicBool,
    input_available: AtomicBool,
    manual_stop_override: AtomicBool,
    pending_manual_start: AtomicBool,
    current_manual: AtomicBool,
    recorded_samples: AtomicU64,
    created_unix_ms: AtomicU64,
    last_output: RwLock<Option<RecordingOutput>>,
    completed_outputs: RwLock<Vec<RecordingOutput>>,
    collect_completed_outputs: AtomicBool,
    error: RwLock<Option<String>>,
}

impl ManagedRecorder {
    pub fn folder(directory: impl AsRef<Path>, format: RecordingFormat, auto_record: bool) -> Self {
        Self::new(
            RecordingDestination::Folder {
                directory: directory.as_ref().to_path_buf(),
                format,
            },
            auto_record,
        )
    }

    pub fn fixed_file(path: impl AsRef<Path>, format: RecordingFormat, auto_record: bool) -> Self {
        Self::new(
            RecordingDestination::FixedFile {
                path: path.as_ref().to_path_buf(),
                format,
            },
            auto_record,
        )
    }

    pub fn replay_cache(
        cache_id: CacheId,
        format: RecordingFormat,
        manager: Arc<ReplayCacheManager>,
        auto_record: bool,
    ) -> Self {
        Self::new(
            RecordingDestination::ReplayCache {
                cache_id,
                format,
                manager,
            },
            auto_record,
        )
    }

    fn new(destination: RecordingDestination, auto_record: bool) -> Self {
        Self {
            destination,
            writer: ArcSwapOption::empty(),
            state: AtomicU8::new(state_to_u8(RecorderState::Disabled)),
            auto_record: AtomicBool::new(auto_record),
            session_active: AtomicBool::new(false),
            input_available: AtomicBool::new(false),
            manual_stop_override: AtomicBool::new(false),
            pending_manual_start: AtomicBool::new(false),
            current_manual: AtomicBool::new(false),
            recorded_samples: AtomicU64::new(0),
            created_unix_ms: AtomicU64::new(0),
            last_output: RwLock::new(None),
            completed_outputs: RwLock::new(Vec::new()),
            collect_completed_outputs: AtomicBool::new(false),
            error: RwLock::new(None),
        }
    }

    pub fn begin_session(&self, input_available: bool) -> Result<()> {
        if self.session_active.swap(true, Ordering::AcqRel) {
            self.set_input_available(input_available)?;
            return Ok(());
        }
        self.input_available
            .store(input_available, Ordering::Release);
        self.manual_stop_override.store(false, Ordering::Release);
        self.pending_manual_start.store(false, Ordering::Release);
        *self.error.write() = None;
        if self.auto_record.load(Ordering::Acquire) {
            if input_available {
                self.start_writer(false)
            } else {
                self.set_state(RecorderState::WaitingForInput);
                Ok(())
            }
        } else {
            self.set_state(RecorderState::Ready);
            Ok(())
        }
    }

    pub fn end_session(&self) -> Result<()> {
        self.session_active.store(false, Ordering::Release);
        self.pending_manual_start.store(false, Ordering::Release);
        self.manual_stop_override.store(false, Ordering::Release);
        let result = self.finish_writer();
        if result.is_ok() {
            self.set_state(RecorderState::Disabled);
        }
        result.map(|_| ())
    }

    pub fn start_manual(&self, input_available: bool) -> Result<()> {
        if !self.session_active.load(Ordering::Acquire) {
            self.begin_session(input_available)?;
        } else {
            self.input_available
                .store(input_available, Ordering::Release);
        }
        self.manual_stop_override.store(false, Ordering::Release);
        if !input_available {
            self.pending_manual_start.store(true, Ordering::Release);
            self.set_state(RecorderState::WaitingForInput);
            return Ok(());
        }
        self.pending_manual_start.store(false, Ordering::Release);
        self.start_writer(true)
    }

    pub fn stop_manual(&self) -> Result<Option<RecordingOutput>> {
        self.pending_manual_start.store(false, Ordering::Release);
        self.manual_stop_override.store(true, Ordering::Release);
        let output = self.finish_writer()?;
        self.set_state(if self.input_available.load(Ordering::Acquire) {
            RecorderState::Ready
        } else {
            RecorderState::WaitingForInput
        });
        Ok(output)
    }

    pub fn set_input_available(&self, available: bool) -> Result<()> {
        let previous = self.input_available.swap(available, Ordering::AcqRel);
        if previous == available || !self.session_active.load(Ordering::Acquire) {
            return Ok(());
        }
        if !available {
            let was_recording = self.state() == RecorderState::Recording;
            let manually_started = was_recording && self.current_manual.load(Ordering::Acquire);
            if was_recording {
                self.finish_writer()?;
            }
            if manually_started {
                self.pending_manual_start.store(true, Ordering::Release);
            }
            self.set_state(RecorderState::WaitingForInput);
            return Ok(());
        }

        let should_start_manual = self.pending_manual_start.swap(false, Ordering::AcqRel);
        let should_start_auto = self.auto_record.load(Ordering::Acquire)
            && !self.manual_stop_override.load(Ordering::Acquire);
        if should_start_manual || should_start_auto {
            self.start_writer(should_start_manual)
        } else {
            self.set_state(RecorderState::Ready);
            Ok(())
        }
    }

    pub fn set_auto_record(&self, auto_record: bool) -> Result<()> {
        self.auto_record.store(auto_record, Ordering::Release);
        if self.session_active.load(Ordering::Acquire)
            && auto_record
            && self.input_available.load(Ordering::Acquire)
            && !self.manual_stop_override.load(Ordering::Acquire)
            && self.writer.load().is_none()
        {
            self.start_writer(false)?;
        } else if !auto_record && self.state() == RecorderState::WaitingForInput {
            self.set_state(RecorderState::Ready);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> RecorderStateSnapshot {
        let writer = self.writer.load_full();
        RecorderStateSnapshot {
            state: self.state(),
            auto_record: self.auto_record.load(Ordering::Acquire),
            session_active: self.session_active.load(Ordering::Acquire),
            input_available: self.input_available.load(Ordering::Acquire),
            manual_stop_override: self.manual_stop_override.load(Ordering::Acquire),
            pending_manual_start: self.pending_manual_start.load(Ordering::Acquire),
            duration_seconds: self.recorded_samples.load(Ordering::Acquire) as f64
                / (INTERNAL_SAMPLE_RATE as f64 * INTERNAL_CHANNELS as f64),
            current_path: writer.as_ref().map(|writer| writer.path().to_path_buf()),
            last_output: self.last_output.read().clone(),
            error: self.error.read().clone(),
        }
    }

    pub fn set_completed_output_collection(&self, enabled: bool) {
        self.collect_completed_outputs
            .store(enabled, Ordering::Release);
        if enabled {
            self.completed_outputs.write().clear();
        }
    }

    pub fn drain_completed_outputs(&self) -> Vec<RecordingOutput> {
        std::mem::take(&mut *self.completed_outputs.write())
    }

    fn start_writer(&self, manually_started: bool) -> Result<()> {
        if self.writer.load().is_some() {
            return Ok(());
        }
        let created_unix_ms = unix_time_ms();
        let (path, format) = match self.next_path(created_unix_ms) {
            Ok(value) => value,
            Err(err) => {
                self.fail(err.to_string());
                return Err(err);
            }
        };
        match RecorderSink::start(path, format, 4) {
            Ok(writer) => {
                self.recorded_samples.store(0, Ordering::Release);
                self.created_unix_ms
                    .store(created_unix_ms, Ordering::Release);
                self.current_manual
                    .store(manually_started, Ordering::Release);
                self.writer.store(Some(Arc::new(writer)));
                *self.error.write() = None;
                self.set_state(RecorderState::Recording);
                Ok(())
            }
            Err(err) => {
                self.fail(err.to_string());
                Err(err)
            }
        }
    }

    fn finish_writer(&self) -> Result<Option<RecordingOutput>> {
        let Some(writer) = self.writer.swap(None) else {
            return Ok(None);
        };
        self.current_manual.store(false, Ordering::Release);
        self.set_state(RecorderState::Stopping);
        if let Err(err) = writer.stop() {
            // Even when finalization fails, the active session has ended.
            // Do not expose the failed take's duration in the next snapshot.
            self.recorded_samples.store(0, Ordering::Release);
            self.fail(err.to_string());
            return Err(err);
        }
        let duration_seconds = self.recorded_samples.load(Ordering::Acquire) as f64
            / (INTERNAL_SAMPLE_RATE as f64 * INTERNAL_CHANNELS as f64);
        // The duration belongs to the completed output.  Clear the active
        // session counter so Ready/Waiting snapshots always start at 00:00.
        self.recorded_samples.store(0, Ordering::Release);
        let path = writer.path().to_path_buf();
        if duration_seconds <= MIN_RECORDED_SECONDS {
            // The encoder has already been finalized, so remove the completed
            // short take rather than leaving an unusable entry in the recorder
            // folder or replay-cache staging area.
            let _ = std::fs::remove_file(&path);
            *self.last_output.write() = None;
            return Ok(None);
        }
        let created_unix_ms = self.created_unix_ms.load(Ordering::Acquire);
        let mut output = RecordingOutput {
            path,
            cache_id: None,
            cache_item_id: None,
            created_unix_ms,
            duration_seconds,
        };
        if let RecordingDestination::ReplayCache {
            cache_id, manager, ..
        } = &self.destination
        {
            let item = match manager.commit_recording(*cache_id, writer.path(), created_unix_ms) {
                Ok(item) => item,
                Err(err) => {
                    self.fail(err.to_string());
                    return Err(err);
                }
            };
            output.path = item.path;
            output.cache_id = Some(*cache_id);
            output.cache_item_id = Some(item.item_id);
            output.duration_seconds = item.duration_seconds;
        }
        *self.last_output.write() = Some(output.clone());
        if self.collect_completed_outputs.load(Ordering::Acquire) {
            self.completed_outputs.write().push(output.clone());
        }
        Ok(Some(output))
    }

    fn next_path(&self, created_unix_ms: u64) -> Result<(PathBuf, RecordingFormat)> {
        match &self.destination {
            RecordingDestination::Folder { directory, format } => {
                std::fs::create_dir_all(directory)?;
                Ok((
                    directory.join(format!(
                        "recording_{created_unix_ms}_{}.{}",
                        uuid::Uuid::new_v4(),
                        format.extension()
                    )),
                    *format,
                ))
            }
            RecordingDestination::FixedFile { path, format } => Ok((path.clone(), *format)),
            RecordingDestination::ReplayCache {
                cache_id,
                format,
                manager,
            } => Ok((manager.recording_staging_path(*cache_id, *format)?, *format)),
        }
    }

    fn state(&self) -> RecorderState {
        u8_to_state(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: RecorderState) {
        self.state.store(state_to_u8(state), Ordering::Release);
    }

    fn fail(&self, message: String) {
        *self.error.write() = Some(message);
        self.set_state(RecorderState::Error);
    }
}

impl AudioSink for ManagedRecorder {
    fn write(&self, block: &AudioBlock) -> Result<()> {
        let Some(writer) = self.writer.load_full() else {
            return Ok(());
        };
        match writer.write(block) {
            Ok(()) => {
                self.recorded_samples
                    .fetch_add(block.samples.len() as u64, Ordering::AcqRel);
                Ok(())
            }
            Err(err) => {
                self.fail(err.to_string());
                Err(err)
            }
        }
    }
}

impl Drop for ManagedRecorder {
    fn drop(&mut self) {
        let _ = self.end_session();
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn state_to_u8(state: RecorderState) -> u8 {
    match state {
        RecorderState::Disabled => 0,
        RecorderState::Ready => 1,
        RecorderState::WaitingForInput => 2,
        RecorderState::Recording => 3,
        RecorderState::Stopping => 4,
        RecorderState::Error => 5,
    }
}

fn u8_to_state(value: u8) -> RecorderState {
    match value {
        1 => RecorderState::Ready,
        2 => RecorderState::WaitingForInput,
        3 => RecorderState::Recording,
        4 => RecorderState::Stopping,
        5 => RecorderState::Error,
        _ => RecorderState::Disabled,
    }
}
