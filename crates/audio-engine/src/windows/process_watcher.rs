use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sysinfo::System;

use crate::{
    audio::graph::{
        ApplicationFilter, ApplicationFilterMode, EndpointCaptureMode, ProcessFilterRule,
        ProcessFilterTarget,
    },
    audio::{buffer::AudioRingBuffer, format::DEFAULT_BLOCK_FRAMES, pacing::BlockPacer},
    error::{BackendError, Result},
    windows::process_loopback::{
        start_process_loopback, ProcessCaptureHandle, ProcessLoopbackMode,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessInfo {
    pub pid: u32,
    pub exe_name: String,
    pub exe_path: Option<PathBuf>,
    pub application_name: String,
    pub window_title: Option<String>,
    pub parent_pid: Option<u32>,
    pub audio_activity: f32,
}

pub fn normalize_exe_path(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

pub fn list_processes() -> Vec<ProcessInfo> {
    let system = System::new_all();
    #[cfg(windows)]
    let audio = active_audio_sessions();
    #[cfg(not(windows))]
    let audio: HashMap<u32, f32> = HashMap::new();

    let mut out: Vec<_> = system
        .processes()
        .values()
        .map(|p| {
            let pid = p.pid().as_u32();
            let exe_path = p.exe().map(Path::to_path_buf);
            let exe_name = p.name().to_string_lossy().into_owned();
            ProcessInfo {
                pid,
                application_name: exe_name.clone(),
                exe_name,
                exe_path,
                window_title: None,
                parent_pid: p.parent().map(|x| x.as_u32()),
                audio_activity: audio.get(&pid).copied().unwrap_or(0.0),
            }
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

#[cfg(windows)]
pub fn active_audio_sessions() -> HashMap<u32, f32> {
    use wasapi::{DeviceEnumerator, Direction};

    let _ = wasapi::initialize_mta();
    let mut result = HashMap::new();
    let Ok(en) = DeviceEnumerator::new() else {
        return result;
    };
    let Ok(devices) = en.get_device_collection(&Direction::Render) else {
        return result;
    };
    let Ok(count) = devices.get_nbr_devices() else {
        return result;
    };
    for i in 0..count {
        let Ok(device) = devices.get_device_at_index(i) else {
            continue;
        };
        let Ok(manager) = device.get_iaudiosessionmanager() else {
            continue;
        };
        let Ok(sessions) = manager.get_audiosessionenumerator() else {
            continue;
        };
        let Ok(n) = sessions.get_count() else {
            continue;
        };
        for j in 0..n {
            let Ok(session) = sessions.get_session(j) else {
                continue;
            };
            let Ok(pid) = session.get_process_id() else {
                continue;
            };
            if pid == 0 {
                continue;
            }
            let peak = session
                .get_audiometerinformation()
                .and_then(|m| m.get_peak_value())
                .unwrap_or(0.0);
            result
                .entry(pid)
                .and_modify(|x| *x = (*x).max(peak))
                .or_insert(peak);
        }
    }
    result
}

#[cfg(not(windows))]
pub fn active_audio_sessions() -> HashMap<u32, f32> {
    HashMap::new()
}

pub fn list_audio_processes() -> Vec<ProcessInfo> {
    let active = active_audio_sessions();
    list_processes()
        .into_iter()
        .filter(|p| active.contains_key(&p.pid))
        .collect()
}

pub fn pids_for_exe(path: &Path) -> Vec<(u32, Option<u32>)> {
    let needle = normalize_exe_path(path);
    list_processes()
        .into_iter()
        .filter_map(|p| {
            let matches =
                p.exe_path.as_deref().map(normalize_exe_path).as_deref() == Some(needle.as_str());
            matches.then_some((p.pid, p.parent_pid))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FilteredCaptureStatus {
    pub capture_mode: EndpointCaptureMode,
    /// Process-loopback capture is system-wide and is not bound to the selected
    /// render endpoint. The frontend must show this semantic difference.
    pub endpoint_bound: bool,
    /// Public Process Loopback can capture process trees, but it cannot expose
    /// the dedicated Windows System Sounds session as an isolated PCM stream.
    pub system_sounds_included: bool,
    pub system_sounds_requested: bool,
    #[serde(default)]
    pub limitations: Vec<String>,
    #[serde(default)]
    pub metrics: ProcessCompositeSnapshot,
}

/// Process capture uses independent producer clocks.  These counters expose
/// the losses and short reads which can otherwise sound like unexplained gaps.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessCompositeSnapshot {
    pub captured_processes: u64,
    pub capture_start_failures: u64,
    pub received_samples: u64,
    pub dropped_samples: u64,
    pub mixed_samples: u64,
    pub output_dropped_samples: u64,
    pub short_read_ticks: u64,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct ProcessCompositeMetrics {
    captured_processes: AtomicU64,
    capture_start_failures: AtomicU64,
    received_samples: AtomicU64,
    dropped_samples: AtomicU64,
    mixed_samples: AtomicU64,
    output_dropped_samples: AtomicU64,
    short_read_ticks: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl ProcessCompositeMetrics {
    fn snapshot(&self) -> ProcessCompositeSnapshot {
        ProcessCompositeSnapshot {
            captured_processes: self.captured_processes.load(Ordering::Acquire),
            capture_start_failures: self.capture_start_failures.load(Ordering::Acquire),
            received_samples: self.received_samples.load(Ordering::Acquire),
            dropped_samples: self.dropped_samples.load(Ordering::Acquire),
            mixed_samples: self.mixed_samples.load(Ordering::Acquire),
            output_dropped_samples: self.output_dropped_samples.load(Ordering::Acquire),
            short_read_ticks: self.short_read_ticks.load(Ordering::Acquire),
            last_error: self.last_error.lock().clone(),
        }
    }
}

struct CapturedProcess {
    handle: ProcessCaptureHandle,
    ring: AudioRingBuffer,
    received_samples: u64,
    dropped_samples: u64,
}

pub struct ProcessCompositeWatcher {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    scan_join: Option<JoinHandle<()>>,
    status: FilteredCaptureStatus,
    metrics: Arc<ProcessCompositeMetrics>,
}

impl ProcessCompositeWatcher {
    pub fn status(&self) -> FilteredCaptureStatus {
        let mut status = self.status.clone();
        status.metrics = self.metrics.snapshot();
        status
    }
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.scan_join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for ProcessCompositeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.scan_join.take() {
            let _ = j.join();
        }
    }
}

pub type ExeCaptureWatcher = ProcessCompositeWatcher;

const COMPOSITE_START_FILL_BLOCKS: usize = 2;

fn composite_has_start_fill(ring: &AudioRingBuffer, block_samples: usize) -> bool {
    ring.len_samples() >= block_samples * COMPOSITE_START_FILL_BLOCKS
}

fn pop_complete_block(ring: &AudioRingBuffer, scratch: &mut [f32], block_samples: usize) -> bool {
    if ring.len_samples() < block_samples {
        return false;
    }
    scratch.fill(0.0);
    let read = ring.pop_into(scratch);
    debug_assert_eq!(read, block_samples);
    read == block_samples
}

#[cfg(windows)]
fn spawn_composite_watcher(
    name: &str,
    output: AudioRingBuffer,
    system_sounds_requested: bool,
    select: impl Fn(&ProcessInfo, &HashSet<u32>) -> bool + Send + 'static,
) -> Result<ProcessCompositeWatcher> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let metrics = Arc::new(ProcessCompositeMetrics::default());
    let worker_metrics = metrics.clone();
    let roots = Arc::new(Mutex::new(HashSet::new()));
    let scan_roots = roots.clone();
    let scan_stop = stop.clone();
    let scan_join = thread::Builder::new()
        .name(format!("{name}-scan"))
        .spawn(move || {
            while !scan_stop.load(Ordering::Acquire) {
                let processes = list_processes();
                let audio_pids: HashSet<u32> = active_audio_sessions().keys().copied().collect();
                let selected: HashMap<u32, ProcessInfo> = processes
                    .into_iter()
                    .filter(|process| select(process, &audio_pids))
                    .map(|process| (process.pid, process))
                    .collect();
                let selected_ids: HashSet<_> = selected.keys().copied().collect();
                let next = selected
                    .values()
                    .filter_map(|process| {
                        let parent_selected = process
                            .parent_pid
                            .is_some_and(|parent| selected_ids.contains(&parent));
                        (!parent_selected).then_some(process.pid)
                    })
                    .collect();
                *scan_roots.lock() = next;
                for _ in 0..75 {
                    if scan_stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        })
        .map_err(BackendError::Io)?;
    let thread_name = name.to_string();
    let join = match thread::Builder::new().name(thread_name).spawn(move || {
        let mut captures: HashMap<u32, CapturedProcess> = HashMap::new();
        let block_samples = DEFAULT_BLOCK_FRAMES * 2;
        let block_duration = Duration::from_secs_f64(DEFAULT_BLOCK_FRAMES as f64 / 48_000.0);
        let mut pacer = BlockPacer::new(Instant::now(), block_duration);
        let mut mix = vec![0.0f32; block_samples];
        let mut scratch = vec![0.0f32; block_samples];
        // Two short blocks absorb the ordinary phase difference between the
        // WASAPI event callback and this 10 ms composite clock. This is
        // bounded startup latency (20 ms), not a growing latency buffer.
        let mut started = false;

        while !worker_stop.load(Ordering::Acquire) {
            let selected_roots = roots.lock().clone();
            captures.retain(|pid, _| selected_roots.contains(pid));
            for pid in selected_roots {
                if captures.contains_key(&pid) {
                    continue;
                }
                let process_ring = AudioRingBuffer::new(48_000 * 2 * 2);
                match start_process_loopback(
                    pid,
                    ProcessLoopbackMode::IncludeTargetTree,
                    process_ring.clone(),
                ) {
                    Ok(handle) => {
                        captures.insert(
                            pid,
                            CapturedProcess {
                                handle,
                                ring: process_ring,
                                received_samples: 0,
                                dropped_samples: 0,
                            },
                        );
                        worker_metrics
                            .captured_processes
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        worker_metrics
                            .capture_start_failures
                            .fetch_add(1, Ordering::Relaxed);
                        *worker_metrics.last_error.lock() = Some(error.to_string());
                    }
                }
            }

            // Aggregate child statistics by delta so ProcessExe diagnostics
            // include failures that occur inside its child capture worker.
            for capture in captures.values_mut() {
                let status = capture.handle.status();
                let received = status.received_samples;
                let dropped = status.dropped_samples;
                worker_metrics.received_samples.fetch_add(
                    received.saturating_sub(capture.received_samples),
                    Ordering::Relaxed,
                );
                worker_metrics.dropped_samples.fetch_add(
                    dropped.saturating_sub(capture.dropped_samples),
                    Ordering::Relaxed,
                );
                capture.received_samples = received;
                capture.dropped_samples = dropped;
                if status.last_error.is_some() {
                    *worker_metrics.last_error.lock() = status.last_error;
                }
            }
            if !started {
                started = captures
                    .values()
                    .any(|capture| composite_has_start_fill(&capture.ring, block_samples));
                if !started {
                    let (wait, _) = pacer.finish_block(Instant::now(), block_duration);
                    if !wait.is_zero() {
                        thread::sleep(wait);
                    }
                    continue;
                }
            }
            mix.fill(0.0);
            let mut had_source = false;
            for capture in captures.values_mut() {
                // A partial packet is retained for the next mix tick.  The
                // old code consumed it and padded the remainder with
                // silence, producing a repeatable 10 ms discontinuity.
                if capture.ring.len_samples() < block_samples {
                    if capture.ring.len_samples() > 0 {
                        worker_metrics
                            .short_read_ticks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                had_source = pop_complete_block(&capture.ring, &mut scratch, block_samples);
                for (dst, src) in mix.iter_mut().zip(scratch.iter()) {
                    *dst += *src;
                }
            }
            if had_source {
                worker_metrics
                    .mixed_samples
                    .fetch_add(block_samples as u64, Ordering::Relaxed);
                let written = output.push_slice(&mix);
                if written < mix.len() {
                    worker_metrics
                        .output_dropped_samples
                        .fetch_add((mix.len() - written) as u64, Ordering::Relaxed);
                }
            }

            let (wait, _) = pacer.finish_block(Instant::now(), block_duration);
            if !wait.is_zero() {
                thread::sleep(wait);
            }
        }
        drop(captures);
    }) {
        Ok(join) => join,
        Err(error) => {
            // 合成workerを起動できなければ、先に作った走査workerを
            // 孤立させない。stopを通知して終了を待ってから失敗を返す。
            stop.store(true, Ordering::Release);
            let _ = scan_join.join();
            return Err(BackendError::Io(error));
        }
    };

    Ok(ProcessCompositeWatcher {
        stop,
        join: Some(join),
        scan_join: Some(scan_join),
        status: FilteredCaptureStatus {
            capture_mode: EndpointCaptureMode::FilteredProcessComposite,
            endpoint_bound: false,
            system_sounds_included: false,
            system_sounds_requested,
            limitations: if system_sounds_requested {
                vec!["Windows System Sounds can be identified as an audio session, but public Process Loopback does not provide that session as an isolated PCM capture target".into()]
            } else {
                Vec::new()
            },
            metrics: ProcessCompositeSnapshot::default(),
        },
        metrics,
    })
}

pub fn start_exe_capture(exe_path: PathBuf, output: AudioRingBuffer) -> Result<ExeCaptureWatcher> {
    #[cfg(not(windows))]
    {
        let _ = (exe_path, output);
        return Err(BackendError::PlatformUnsupported(
            "EXE process loopback requires Windows".into(),
        ));
    }
    #[cfg(windows)]
    {
        let needle = normalize_exe_path(&exe_path);
        spawn_composite_watcher("exe-process-composite", output, false, move |p, _audio| {
            p.exe_path.as_deref().map(normalize_exe_path).as_deref() == Some(needle.as_str())
        })
    }
}

pub fn start_filtered_process_composite(
    filter: ApplicationFilter,
    output: AudioRingBuffer,
) -> Result<ProcessCompositeWatcher> {
    if filter.mode == ApplicationFilterMode::Disabled {
        return Err(BackendError::InvalidOperation(
            "disabled application filter must use endpoint loopback".into(),
        ));
    }
    #[cfg(not(windows))]
    {
        let _ = (filter, output);
        return Err(BackendError::PlatformUnsupported(
            "filtered process capture requires Windows".into(),
        ));
    }
    #[cfg(windows)]
    {
        let system_rule = filter
            .rules
            .iter()
            .any(|r| r.enabled && matches!(r.target, ProcessFilterTarget::SystemSounds));
        let legacy_paths: HashSet<String> = filter
            .legacy_exe_paths
            .iter()
            .map(|p| normalize_exe_path(p))
            .collect();
        let rules = filter.rules.clone();
        let mode = filter.mode;
        spawn_composite_watcher(
            match mode {
                ApplicationFilterMode::Whitelist => "process-filter-whitelist",
                ApplicationFilterMode::ExcludeList => "process-filter-exclude",
                ApplicationFilterMode::Disabled => unreachable!(),
            },
            output,
            match mode {
                ApplicationFilterMode::Whitelist => system_rule,
                // In exclude mode a SystemSounds rule means omission is exactly
                // what the caller requested. Without the rule, omission is a
                // capture limitation and is reported by status.
                ApplicationFilterMode::ExcludeList => !system_rule,
                ApplicationFilterMode::Disabled => false,
            },
            move |p, audio| {
                if !audio.contains(&p.pid) {
                    return false;
                }
                let matched = legacy_paths.contains(
                    p.exe_path
                        .as_deref()
                        .map(normalize_exe_path)
                        .as_deref()
                        .unwrap_or(""),
                ) || rules
                    .iter()
                    .any(|rule| rule.enabled && rule_matches_process(rule, p));
                match mode {
                    ApplicationFilterMode::Whitelist => matched,
                    ApplicationFilterMode::ExcludeList => !matched,
                    ApplicationFilterMode::Disabled => false,
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_shifted_process_packets_keep_complete_pcm_blocks() {
        let block_samples = DEFAULT_BLOCK_FRAMES * 2;
        let ring = AudioRingBuffer::new(block_samples * 4);
        let input: Vec<f32> = (0..block_samples * 3).map(|n| n as f32).collect();

        // The producer is half a block behind the composite tick. A consumer
        // must not emit that partial block followed by zero padding.
        assert_eq!(
            ring.push_slice(&input[..block_samples / 2]),
            block_samples / 2
        );
        assert!(!composite_has_start_fill(&ring, block_samples));
        let mut scratch = vec![0.0; block_samples];
        assert!(!pop_complete_block(&ring, &mut scratch, block_samples));

        assert_eq!(
            ring.push_slice(&input[block_samples / 2..]),
            input.len() - block_samples / 2
        );
        assert!(composite_has_start_fill(&ring, block_samples));
        assert!(pop_complete_block(&ring, &mut scratch, block_samples));
        assert_eq!(scratch, input[..block_samples]);
        assert!(pop_complete_block(&ring, &mut scratch, block_samples));
        assert_eq!(scratch, input[block_samples..block_samples * 2]);
        assert!(pop_complete_block(&ring, &mut scratch, block_samples));
        assert_eq!(scratch, input[block_samples * 2..]);
    }
}

fn rule_matches_process(rule: &ProcessFilterRule, process: &ProcessInfo) -> bool {
    match &rule.target {
        ProcessFilterTarget::Pid(pid) => process.pid == *pid,
        ProcessFilterTarget::ExeName(name) => process.exe_name.eq_ignore_ascii_case(name),
        ProcessFilterTarget::ExePath(path) => {
            process
                .exe_path
                .as_deref()
                .map(normalize_exe_path)
                .as_deref()
                == Some(normalize_exe_path(path).as_str())
        }
        ProcessFilterTarget::SystemSounds => false,
    }
}
