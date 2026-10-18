use crate::{
    audio::{buffer::AudioRingBuffer, format::INTERNAL_SAMPLE_RATE},
    error::{BackendError, Result},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLoopbackMode {
    IncludeTargetTree,
    ExcludeTargetTree,
}

/// Counters are intentionally collected outside the audio graph.  They make a
/// process-loopback failure visible without doing logging from the capture
/// callback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessCaptureSnapshot {
    pub received_samples: u64,
    pub dropped_samples: u64,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct ProcessCaptureStatus {
    received_samples: AtomicU64,
    dropped_samples: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl ProcessCaptureStatus {
    fn snapshot(&self) -> ProcessCaptureSnapshot {
        ProcessCaptureSnapshot {
            received_samples: self.received_samples.load(Ordering::Acquire),
            dropped_samples: self.dropped_samples.load(Ordering::Acquire),
            last_error: self.last_error.lock().clone(),
        }
    }
}

pub struct ProcessCaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    status: Arc<ProcessCaptureStatus>,
}
impl ProcessCaptureHandle {
    pub fn status(&self) -> ProcessCaptureSnapshot {
        self.status.snapshot()
    }
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
impl Drop for ProcessCaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(windows)]
pub fn start_process_loopback(
    pid: u32,
    mode: ProcessLoopbackMode,
    ring: AudioRingBuffer,
) -> Result<ProcessCaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let status = Arc::new(ProcessCaptureStatus::default());
    let worker_status = status.clone();
    let join = thread::Builder::new()
        .name(format!("process-loopback-{pid}"))
        .spawn(move || {
            use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
            let _ = wasapi::initialize_mta();
            let run = || -> std::result::Result<(), String> {
                let include = matches!(mode, ProcessLoopbackMode::IncludeTargetTree);
                let mut client = AudioClient::new_application_loopback_client(pid, include)
                    .map_err(|e| e.to_string())?;
                let fmt = WaveFormat::new(
                    32,
                    32,
                    &SampleType::Float,
                    INTERNAL_SAMPLE_RATE as usize,
                    2,
                    None,
                );
                let stream_mode = StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 200_000,
                };
                client
                    .initialize_client(&fmt, &Direction::Capture, &stream_mode)
                    .map_err(|e| e.to_string())?;
                let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
                let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
                client.start_stream().map_err(|e| e.to_string())?;
                // Reused packet buffers keep the steady-state capture path allocation-free.
                // They grow only if a driver reports an unexpectedly larger packet.
                let mut bytes = vec![0u8; 4096 * 8];
                let mut decoded = vec![0.0f32; 4096 * 2];
                while !worker_stop.load(Ordering::Acquire) {
                    if event.wait_for_event(100).is_err() {
                        continue;
                    }
                    loop {
                        let frames =
                            match capture.get_next_packet_size().map_err(|e| e.to_string())? {
                                Some(n) if n > 0 => n as usize,
                                _ => break,
                            };
                        let needed_bytes = frames * 8;
                        let needed_samples = frames * 2;
                        if bytes.len() < needed_bytes {
                            bytes.resize(needed_bytes, 0);
                        }
                        if decoded.len() < needed_samples {
                            decoded.resize(needed_samples, 0.0);
                        }
                        let (read, info) = capture
                            .read_from_device(&mut bytes[..needed_bytes])
                            .map_err(|e| e.to_string())?;
                        let sample_count = read as usize * 2;
                        if info.flags.silent {
                            decoded[..sample_count].fill(0.0);
                        } else {
                            for (dst, chunk) in decoded[..sample_count]
                                .iter_mut()
                                .zip(bytes[..sample_count * 4].chunks_exact(4))
                            {
                                *dst = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                            }
                        }
                        worker_status
                            .received_samples
                            .fetch_add(sample_count as u64, Ordering::Relaxed);
                        let written = ring.push_slice(&decoded[..sample_count]);
                        if written < sample_count {
                            worker_status
                                .dropped_samples
                                .fetch_add((sample_count - written) as u64, Ordering::Relaxed);
                        }
                    }
                }
                let _ = client.stop_stream();
                Ok(())
            };
            if let Err(error) = run() {
                *worker_status.last_error.lock() = Some(error);
            }
            wasapi::deinitialize();
        })
        .map_err(BackendError::Io)?;
    Ok(ProcessCaptureHandle {
        stop,
        join: Some(join),
        status,
    })
}

#[cfg(not(windows))]
pub fn start_process_loopback(
    _pid: u32,
    _mode: ProcessLoopbackMode,
    _ring: AudioRingBuffer,
) -> Result<ProcessCaptureHandle> {
    Err(BackendError::PlatformUnsupported(
        "process loopback requires Windows 10 build 20348+".into(),
    ))
}
