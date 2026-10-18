use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};

use crate::audio::graph::FolderReplayMode;
use crate::{
    audio::{buffer::AudioRingBuffer, engine::AudioSource, format::AudioBlock},
    error::{BackendError, Result},
    recording::codecs::decode_audio_file,
    replay::{
        player::{duration_to_sample, sample_to_duration, ReplaySource},
        state::{ReplayPlaybackState, ReplayStateSnapshot},
    },
};

enum FolderCommand {
    Play,
    Pause,
    Stop,
    Seek(u64),
    Select(PathBuf),
    SetMode(FolderReplayMode),
    Refresh,
    Shutdown,
}

pub struct FolderReplayPlayer {
    folder: PathBuf,
    ring: AudioRingBuffer,
    command: Sender<FolderCommand>,
    state: Arc<AtomicU8>,
    position_samples: Arc<AtomicU64>,
    duration_samples: Arc<AtomicU64>,
    eof: Arc<AtomicBool>,
    files: Arc<RwLock<Vec<PathBuf>>>,
    current_file: Arc<RwLock<Option<PathBuf>>>,
    revision: Arc<AtomicU64>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl FolderReplayPlayer {
    pub fn open(
        folder: impl AsRef<Path>,
        mode: FolderReplayMode,
        selected_file: Option<PathBuf>,
    ) -> Result<Self> {
        let folder = folder.as_ref().to_path_buf();
        let initial_files = scan_folder(&folder)?;
        let ring = AudioRingBuffer::new(48_000 * 2 * 2);
        let (command, receiver) = crossbeam_channel::unbounded();
        let state = Arc::new(AtomicU8::new(ReplayPlaybackState::Stopped.as_u8()));
        let position_samples = Arc::new(AtomicU64::new(0));
        let duration_samples = Arc::new(AtomicU64::new(0));
        let eof = Arc::new(AtomicBool::new(false));
        let files = Arc::new(RwLock::new(initial_files));
        let current_file = Arc::new(RwLock::new(None));
        let revision = Arc::new(AtomicU64::new(0));
        let join = spawn_worker(
            folder.clone(),
            ring.clone(),
            receiver,
            state.clone(),
            position_samples.clone(),
            duration_samples.clone(),
            eof.clone(),
            files.clone(),
            current_file.clone(),
            revision.clone(),
            mode,
            selected_file,
        )?;
        Ok(Self {
            folder,
            ring,
            command,
            state,
            position_samples,
            duration_samples,
            eof,
            files,
            current_file,
            revision,
            join: Mutex::new(Some(join)),
        })
    }

    pub fn files(&self) -> Vec<PathBuf> {
        self.files.read().clone()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn refresh_files(&self) -> Result<Vec<PathBuf>> {
        let latest = scan_folder(&self.folder)?;
        update_catalog(&self.files, &self.revision, latest.clone());
        let _ = self.command.send(FolderCommand::Refresh);
        Ok(latest)
    }

    pub fn select_file(&self, path: PathBuf) -> Result<()> {
        let absolute = if path.is_absolute() {
            path
        } else {
            self.folder.join(path)
        };
        if !self.files.read().contains(&absolute) {
            return Err(BackendError::InvalidOperation(
                "selected file is not in the replay folder".into(),
            ));
        }
        self.command
            .send(FolderCommand::Select(absolute))
            .map_err(|_| BackendError::InvalidOperation("folder replay worker stopped".into()))
    }

    pub fn set_mode(&self, mode: FolderReplayMode) -> Result<()> {
        self.command
            .send(FolderCommand::SetMode(mode))
            .map_err(|_| BackendError::InvalidOperation("folder replay worker stopped".into()))
    }

    fn playback_state(&self) -> ReplayPlaybackState {
        ReplayPlaybackState::from_u8(self.state.load(Ordering::Acquire))
    }
}

impl ReplaySource for FolderReplayPlayer {
    fn play(&self) -> Result<()> {
        send(&self.command, FolderCommand::Play)
    }
    fn pause(&self) -> Result<()> {
        send(&self.command, FolderCommand::Pause)
    }
    fn stop(&self) -> Result<()> {
        send(&self.command, FolderCommand::Stop)
    }
    fn seek(&self, position: Duration) -> Result<()> {
        send(
            &self.command,
            FolderCommand::Seek(duration_to_sample(position)),
        )
    }
    fn position(&self) -> Duration {
        sample_to_duration(self.position_samples.load(Ordering::Acquire))
    }
    fn duration(&self) -> Option<Duration> {
        let samples = self.duration_samples.load(Ordering::Acquire);
        (samples > 0).then(|| sample_to_duration(samples))
    }
    fn snapshot(&self) -> ReplayStateSnapshot {
        ReplayStateSnapshot {
            state: self.playback_state(),
            position_seconds: self.position().as_secs_f64(),
            duration_seconds: self.duration().map(|duration| duration.as_secs_f64()),
            eof: self.eof.load(Ordering::Acquire),
            current_file: self
                .current_file
                .read()
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
        }
    }
}

impl AudioSource for FolderReplayPlayer {
    fn replay_snapshot(&self) -> Option<ReplayStateSnapshot> {
        Some(self.snapshot())
    }

    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        if self.playback_state() != ReplayPlaybackState::Playing {
            return Ok(());
        }
        let read = self.ring.pop_into(&mut block.samples);
        self.position_samples
            .fetch_add(read as u64, Ordering::AcqRel);
        if read == 0 && self.eof.load(Ordering::Acquire) {
            self.state
                .store(ReplayPlaybackState::Stopped.as_u8(), Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for FolderReplayPlayer {
    fn drop(&mut self) {
        let _ = self.command.send(FolderCommand::Shutdown);
        if let Some(join) = self.join.lock().take() {
            let _ = join.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    folder: PathBuf,
    ring: AudioRingBuffer,
    receiver: Receiver<FolderCommand>,
    state_atomic: Arc<AtomicU8>,
    position_atomic: Arc<AtomicU64>,
    duration_atomic: Arc<AtomicU64>,
    eof_atomic: Arc<AtomicBool>,
    files_shared: Arc<RwLock<Vec<PathBuf>>>,
    current_shared: Arc<RwLock<Option<PathBuf>>>,
    revision: Arc<AtomicU64>,
    initial_mode: FolderReplayMode,
    initial_selected: Option<PathBuf>,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("folder-replay-worker".into())
        .spawn(move || {
            let mut mode = initial_mode;
            let mut selected = initial_selected.map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    folder.join(path)
                }
            });
            let mut current: Option<PathBuf> = None;
            let mut samples = Vec::<f32>::new();
            let mut cursor = 0usize;
            let mut last_scan = Instant::now() - Duration::from_secs(1);
            loop {
                if last_scan.elapsed() >= Duration::from_millis(100) {
                    if let Ok(latest) = scan_folder(&folder) {
                        update_catalog(&files_shared, &revision, latest);
                    }
                    last_scan = Instant::now();
                }
                while let Ok(command) = receiver.try_recv() {
                    match command {
                        FolderCommand::Play => {
                            if samples.is_empty() || cursor >= samples.len() {
                                let path =
                                    choose_initial(&files_shared.read(), mode, selected.as_ref());
                                if let Some(path) = path {
                                    if let Ok(decoded) = decode_audio_file(&path) {
                                        samples = decoded.samples;
                                        cursor = 0;
                                        current = Some(path.clone());
                                        *current_shared.write() = Some(path);
                                        position_atomic.store(0, Ordering::Release);
                                        duration_atomic
                                            .store(samples.len() as u64, Ordering::Release);
                                    }
                                }
                            }
                            eof_atomic.store(false, Ordering::Release);
                            state_atomic
                                .store(ReplayPlaybackState::Playing.as_u8(), Ordering::Release);
                        }
                        FolderCommand::Pause => state_atomic
                            .store(ReplayPlaybackState::Paused.as_u8(), Ordering::Release),
                        FolderCommand::Stop => {
                            state_atomic
                                .store(ReplayPlaybackState::Stopped.as_u8(), Ordering::Release);
                            cursor = 0;
                            ring.clear();
                            position_atomic.store(0, Ordering::Release);
                            eof_atomic.store(false, Ordering::Release);
                        }
                        FolderCommand::Seek(sample) => {
                            cursor = (sample as usize).min(samples.len());
                            ring.clear();
                            position_atomic.store(cursor as u64, Ordering::Release);
                            eof_atomic.store(false, Ordering::Release);
                        }
                        FolderCommand::Select(path) => {
                            selected = Some(path.clone());
                            if mode == FolderReplayMode::SelectedFileOnly {
                                if let Ok(decoded) = decode_audio_file(&path) {
                                    samples = decoded.samples;
                                    cursor = 0;
                                    current = Some(path.clone());
                                    *current_shared.write() = Some(path);
                                    ring.clear();
                                    position_atomic.store(0, Ordering::Release);
                                    duration_atomic.store(samples.len() as u64, Ordering::Release);
                                }
                            }
                        }
                        FolderCommand::SetMode(next) => mode = next,
                        FolderCommand::Refresh => {
                            last_scan = Instant::now() - Duration::from_secs(1)
                        }
                        FolderCommand::Shutdown => return,
                    }
                }
                if ReplayPlaybackState::from_u8(state_atomic.load(Ordering::Acquire))
                    != ReplayPlaybackState::Playing
                {
                    thread::sleep(Duration::from_millis(3));
                    continue;
                }
                if cursor >= samples.len() {
                    if ring.len_samples() > 0 {
                        thread::sleep(Duration::from_millis(3));
                        continue;
                    }
                    if mode == FolderReplayMode::Sequential {
                        let catalog = files_shared.read().clone();
                        if let Some(path) = choose_next(&catalog, current.as_ref()) {
                            match decode_audio_file(&path) {
                                Ok(decoded) => {
                                    samples = decoded.samples;
                                    cursor = 0;
                                    current = Some(path.clone());
                                    *current_shared.write() = Some(path);
                                    ring.clear();
                                    position_atomic.store(0, Ordering::Release);
                                    duration_atomic.store(samples.len() as u64, Ordering::Release);
                                    eof_atomic.store(false, Ordering::Release);
                                }
                                Err(_) => thread::sleep(Duration::from_millis(10)),
                            }
                        } else {
                            eof_atomic.store(true, Ordering::Release);
                            thread::sleep(Duration::from_millis(3));
                        }
                    } else {
                        eof_atomic.store(true, Ordering::Release);
                        thread::sleep(Duration::from_millis(3));
                    }
                    continue;
                }
                let end = (cursor + 8192).min(samples.len());
                let pushed = ring.push_slice(&samples[cursor..end]);
                cursor += pushed;
                if pushed == 0 {
                    thread::sleep(Duration::from_millis(3));
                }
            }
        })
        .map_err(BackendError::Io)
}

fn send(sender: &Sender<FolderCommand>, command: FolderCommand) -> Result<()> {
    sender
        .send(command)
        .map_err(|_| BackendError::InvalidOperation("folder replay worker stopped".into()))
}

fn choose_initial(
    files: &[PathBuf],
    mode: FolderReplayMode,
    selected: Option<&PathBuf>,
) -> Option<PathBuf> {
    if mode == FolderReplayMode::SelectedFileOnly {
        selected.filter(|path| files.contains(path)).cloned()
    } else {
        files.first().cloned()
    }
}

fn choose_next(files: &[PathBuf], current: Option<&PathBuf>) -> Option<PathBuf> {
    if files.is_empty() {
        return None;
    }
    let Some(current) = current else {
        return files.first().cloned();
    };
    if let Some(index) = files.iter().position(|path| path == current) {
        return files.get(index + 1).cloned();
    }
    files
        .iter()
        .find(|path| *path > current)
        .cloned()
        .or_else(|| files.first().cloned())
}

fn update_catalog(files: &RwLock<Vec<PathBuf>>, revision: &AtomicU64, latest: Vec<PathBuf>) {
    let mut current = files.write();
    if *current != latest {
        *current = latest;
        revision.fetch_add(1, Ordering::AcqRel);
    }
}

pub fn scan_folder(folder: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let folder = folder.as_ref();
    if !folder.is_dir() {
        return Err(BackendError::InvalidOperation(format!(
            "replay folder does not exist: {}",
            folder.display()
        )));
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && matches!(
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
                    .as_deref(),
                Some("wav" | "flac" | "mp3")
            )
        {
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}
