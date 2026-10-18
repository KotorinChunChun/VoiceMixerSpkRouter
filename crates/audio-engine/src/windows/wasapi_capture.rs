use crate::{
    audio::{
        buffer::AudioRingBuffer, clock::CaptureClock, graph::CaptureMethod, level_probe::LevelProbe,
    },
    error::{BackendError, Result},
    windows::device_format::DeviceFormat,
    windows::worker_status::{DeviceWorkerSnapshot, DeviceWorkerStatus},
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    status: DeviceWorkerStatus,
    dropped_samples: Arc<AtomicU64>,
    clock: Arc<CaptureClock>,
    level_probe: Arc<LevelProbe>,
}

pub struct MeterCaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    status: DeviceWorkerStatus,
    meter: Arc<crate::audio::meter::AtomicMeter>,
}

impl MeterCaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    pub fn status(&self) -> DeviceWorkerSnapshot {
        self.status.snapshot()
    }

    pub fn meter(&self) -> crate::audio::meter::MeterFrame {
        self.meter.load()
    }
}

impl Drop for MeterCaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
    pub fn status(&self) -> DeviceWorkerSnapshot {
        self.status.snapshot()
    }
    pub fn wait_until_running(&self, timeout: Duration) -> Result<DeviceWorkerSnapshot> {
        self.status.wait_until_running(timeout)
    }
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Acquire)
    }
    pub fn clock(&self) -> Arc<CaptureClock> {
        self.clock.clone()
    }
    pub fn level_probe(&self) -> Arc<LevelProbe> {
        self.level_probe.clone()
    }
}
impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn capture_method_name(method: CaptureMethod) -> &'static str {
    match method {
        CaptureMethod::Auto => "auto",
        CaptureMethod::WasapiSharedEvent => "wasapi_shared_event",
        CaptureMethod::WasapiSharedPolling => "wasapi_shared_polling",
        CaptureMethod::WasapiExclusiveEvent => "wasapi_exclusive_event",
        CaptureMethod::WasapiExclusivePolling => "wasapi_exclusive_polling",
        CaptureMethod::WinMmWaveIn => "win_mm_wave_in",
    }
}

#[cfg(windows)]
fn capture_candidates(
    requested: CaptureMethod,
    auto_priority: &[CaptureMethod],
) -> Vec<CaptureMethod> {
    use CaptureMethod::*;
    match requested {
        Auto => {
            let mut candidates: Vec<_> = auto_priority
                .iter()
                .copied()
                .filter(|method| *method != Auto)
                .collect();
            for fallback in [WasapiSharedEvent, WasapiSharedPolling, WinMmWaveIn] {
                if !candidates.contains(&fallback) {
                    candidates.push(fallback);
                }
            }
            candidates
        }
        WasapiSharedEvent => vec![WasapiSharedEvent],
        WasapiSharedPolling => vec![WasapiSharedPolling],
        WasapiExclusiveEvent => vec![WasapiExclusiveEvent],
        WasapiExclusivePolling => vec![WasapiExclusivePolling],
        WinMmWaveIn => vec![WinMmWaveIn],
    }
}

#[cfg(windows)]
fn run_wasapi_capture(
    endpoint_id: &str,
    method: CaptureMethod,
    stop: &Arc<AtomicBool>,
    status: &DeviceWorkerStatus,
    clock: &CaptureClock,
    mut on_samples: impl FnMut(&[f32]),
) -> std::result::Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    let en = DeviceEnumerator::new().map_err(|e| format!("DeviceEnumerator::new: {e}"))?;
    let device = en
        .get_device(endpoint_id)
        .map_err(|e| format!("get_device: {e}"))?;
    let mut client = device
        .get_iaudioclient()
        .map_err(|e| format!("get_iaudioclient: {e}"))?;
    let mix_fmt = client
        .get_mixformat()
        .map_err(|e| format!("get_mixformat: {e}"))?;
    let exclusive = matches!(
        method,
        CaptureMethod::WasapiExclusiveEvent | CaptureMethod::WasapiExclusivePolling
    );
    let fmt = if matches!(
        method,
        CaptureMethod::WasapiSharedEvent | CaptureMethod::WasapiSharedPolling
    ) {
        // GetMixFormat is the format the Windows audio engine guarantees for
        // shared mode. DeviceFormat converts/resamples it into the internal
        // 48 kHz stereo representation after capture.
        mix_fmt.clone()
    } else if exclusive {
        // The shared-mode mix format is not necessarily accepted by the device
        // in exclusive mode. Query the driver with common native formats and
        // use the first format it explicitly accepts.
        let mut candidates = vec![mix_fmt.clone()];
        let mut sample_rates = vec![mix_fmt.get_samplespersec()];
        for sample_rate in [48_000, 44_100, 96_000] {
            if !sample_rates.contains(&sample_rate) {
                sample_rates.push(sample_rate);
            }
        }
        let mut channels = vec![mix_fmt.get_nchannels()];
        for channel_count in [2, 1] {
            if !channels.contains(&channel_count) {
                channels.push(channel_count);
            }
        }
        for sample_rate in sample_rates {
            for channel_count in &channels {
                let sample_rate = sample_rate as usize;
                let channel_count = *channel_count as usize;
                candidates.extend([
                    WaveFormat::new(32, 32, &SampleType::Float, sample_rate, channel_count, None),
                    WaveFormat::new(32, 24, &SampleType::Int, sample_rate, channel_count, None),
                    WaveFormat::new(24, 24, &SampleType::Int, sample_rate, channel_count, None),
                    WaveFormat::new(16, 16, &SampleType::Int, sample_rate, channel_count, None),
                ]);
            }
        }
        let mut errors = Vec::new();
        let mut supported = None;
        for candidate in candidates {
            match client.is_supported_exclusive_with_quirks(&candidate) {
                Ok(format) => {
                    supported = Some(format);
                    break;
                }
                Err(error) => errors.push(format!("{candidate:?}: {error}")),
            }
        }
        supported.ok_or_else(|| {
            format!(
                "exclusive format negotiation failed; mix={mix_fmt:?}; tried={}",
                errors.join(" | ")
            )
        })?
    } else {
        return Err("invalid WASAPI capture method".into());
    };
    let device_format =
        DeviceFormat::from_wave_format(&fmt).map_err(|e| format!("device format: {e}"))?;
    let (default_period, minimum_period) = client
        .get_device_period()
        .map_err(|e| format!("get_device_period: {e}"))?;
    let exclusive_period = if exclusive {
        Some(
            client
                .calculate_aligned_period_near(default_period, Some(128), &fmt)
                .map_err(|e| format!("calculate_aligned_period_near: {e}"))?,
        )
    } else {
        None
    };
    let mode = match method {
        CaptureMethod::WasapiSharedEvent => StreamMode::EventsShared {
            autoconvert: false,
            buffer_duration_hns: default_period,
        },
        CaptureMethod::WasapiSharedPolling => StreamMode::PollingShared {
            autoconvert: false,
            buffer_duration_hns: default_period,
        },
        CaptureMethod::WasapiExclusiveEvent => StreamMode::EventsExclusive {
            period_hns: exclusive_period.expect("exclusive period"),
        },
        CaptureMethod::WasapiExclusivePolling => StreamMode::PollingExclusive {
            buffer_duration_hns: 4 * exclusive_period.expect("exclusive period"),
            period_hns: exclusive_period.expect("exclusive period"),
        },
        CaptureMethod::Auto | CaptureMethod::WinMmWaveIn => {
            return Err("invalid WASAPI capture method".into())
        }
    };
    client
        .initialize_client(&fmt, &Direction::Capture, &mode)
        .map_err(|e| {
            format!(
                "initialize_client: {e}; endpoint={endpoint_id}; requested={fmt:?}; mix={mix_fmt:?}; default_period_hns={default_period}; minimum_period_hns={minimum_period}; mode={mode:?}"
            )
        })?;
    let event = if matches!(
        method,
        CaptureMethod::WasapiSharedEvent | CaptureMethod::WasapiExclusiveEvent
    ) {
        Some(
            client
                .set_get_eventhandle()
                .map_err(|e| format!("set_get_eventhandle: {e}"))?,
        )
    } else {
        None
    };
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| format!("get_audiocaptureclient: {e}"))?;
    let max_frames = client.get_buffer_size().unwrap_or(4096).max(1) as usize;
    let mut bytes = vec![
        0u8;
        device_format
            .bytes_for_frames(max_frames)
            .map_err(|e| e.to_string())?
    ];
    client
        .start_stream()
        .map_err(|e| format!("start_stream: {e}"))?;
    status.running_with_method(capture_method_name(method));

    let poll_delay = Duration::from_micros((default_period.max(10_000) as u64) / 10);
    while !stop.load(Ordering::Acquire) {
        if let Some(event) = &event {
            if event.wait_for_event(100).is_err() {
                continue;
            }
        } else {
            thread::sleep(poll_delay.min(Duration::from_millis(20)));
        }
        loop {
            let frames = match capture.get_next_packet_size().map_err(|e| e.to_string())? {
                Some(n) if n > 0 => n as usize,
                _ => break,
            };
            if frames > max_frames {
                return Err("capture packet exceeded WASAPI buffer size".into());
            }
            let packet_bytes = device_format
                .bytes_for_frames(frames)
                .map_err(|e| e.to_string())?;
            let (read, info) = capture
                .read_from_device(&mut bytes[..packet_bytes])
                .map_err(|e| e.to_string())?;
            let (ticks, freq) = crate::audio::clock::qpc_counter();
            let (time, accuracy) = crate::audio::clock::resolve_capture_timestamp(
                info.timestamp,
                info.flags.timestamp_error,
                ticks,
                freq,
            );
            clock.observe(time, accuracy);
            let decoded = device_format
                .decode_to_internal(&bytes, read as usize, info.flags.silent)
                .map_err(|e| e.to_string())?;
            on_samples(&decoded);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

#[cfg(windows)]
pub fn start_shared_meter(endpoint_id: impl Into<String>) -> Result<MeterCaptureHandle> {
    let endpoint_id = endpoint_id.into();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let status = DeviceWorkerStatus::capture("wasapi_shared_event");
    let worker_status = status.clone();
    let meter = Arc::new(crate::audio::meter::AtomicMeter::default());
    let worker_meter = meter.clone();
    let join = thread::Builder::new()
        .name("wasapi-shared-meter".into())
        .spawn(move || {
            let _ = wasapi::initialize_mta();
            while !worker_stop.load(Ordering::Acquire) {
                let mut failures = Vec::new();
                for method in [
                    CaptureMethod::WasapiSharedEvent,
                    CaptureMethod::WasapiSharedPolling,
                ] {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let meter_clock = CaptureClock::default();
                    let result = run_wasapi_capture(
                        &endpoint_id,
                        method,
                        &worker_stop,
                        &worker_status,
                        &meter_clock,
                        |samples| worker_meter.store(crate::audio::meter::measure(samples)),
                    );
                    match result {
                        Ok(()) if worker_stop.load(Ordering::Acquire) => break,
                        Ok(()) => failures.push(format!(
                            "{} stopped unexpectedly",
                            capture_method_name(method)
                        )),
                        Err(error) => {
                            failures.push(format!("{}: {error}", capture_method_name(method)))
                        }
                    }
                }
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                worker_status.reconnecting(failures.join(" | "));
                thread::sleep(Duration::from_millis(500));
            }
            worker_status.stopped();
            wasapi::deinitialize();
        })
        .map_err(BackendError::Io)?;
    Ok(MeterCaptureHandle {
        stop,
        join: Some(join),
        status,
        meter,
    })
}

#[cfg(windows)]
fn spawn_endpoint_capture(
    endpoint_id: String,
    loopback: bool,
    requested_method: CaptureMethod,
    ring: AudioRingBuffer,
    auto_priority: Vec<CaptureMethod>,
) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let status = DeviceWorkerStatus::capture(capture_method_name(requested_method));
    let worker_status = status.clone();
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let worker_dropped_samples = dropped_samples.clone();
    let clock = Arc::new(CaptureClock::default());
    let worker_clock = clock.clone();
    let level_probe = Arc::new(LevelProbe::default());
    let worker_probe = level_probe.clone();
    let join = thread::Builder::new()
        .name(if loopback {
            "wasapi-loopback-capture".into()
        } else {
            "microphone-capture".into()
        })
        .spawn(move || {
            let _ = wasapi::initialize_mta();
            while !worker_stop.load(Ordering::Acquire) {
                let mut failures = Vec::new();
                for method in capture_candidates(requested_method, &auto_priority) {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    worker_probe.restart();
                    let result = if method == CaptureMethod::WinMmWaveIn {
                        crate::windows::winmm_capture::run_winmm_capture(
                            &endpoint_id,
                            &ring,
                            &worker_stop,
                            &worker_dropped_samples,
                            worker_clock.as_ref(),
                            worker_probe.as_ref(),
                            || worker_status.running_with_method(capture_method_name(method)),
                        )
                        .map_err(|error| error.to_string())
                    } else {
                        run_wasapi_capture(
                            &endpoint_id,
                            method,
                            &worker_stop,
                            &worker_status,
                            worker_clock.as_ref(),
                            |samples| {
                                if let Some((at, _)) = worker_clock.stamp() {
                                    worker_probe.observe(samples, at, 48_000, 2);
                                }
                                let written = ring.push_slice(samples);
                                if written < samples.len() {
                                    worker_dropped_samples.fetch_add(
                                        (samples.len() - written) as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                            },
                        )
                    };
                    match result {
                        Ok(()) if worker_stop.load(Ordering::Acquire) => break,
                        Ok(()) => failures.push(format!(
                            "{} stopped unexpectedly",
                            capture_method_name(method)
                        )),
                        Err(error) => {
                            failures.push(format!("{}: {error}", capture_method_name(method)))
                        }
                    }
                }
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                worker_status.reconnecting(failures.join(" | "));
                thread::sleep(Duration::from_millis(500));
            }
            worker_status.stopped();
            wasapi::deinitialize();
        })
        .map_err(BackendError::Io)?;
    Ok(CaptureHandle {
        stop,
        join: Some(join),
        status,
        dropped_samples,
        clock,
        level_probe,
    })
}

#[cfg(windows)]
pub fn start_microphone_capture_with_method(
    endpoint_id: impl Into<String>,
    method: CaptureMethod,
    ring: AudioRingBuffer,
) -> Result<CaptureHandle> {
    start_microphone_capture_with_auto_priority(endpoint_id, method, ring, Vec::new())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn auto_capture_candidates_always_keep_safe_fallbacks() {
        assert_eq!(
            capture_candidates(CaptureMethod::Auto, &[CaptureMethod::WasapiExclusiveEvent]),
            vec![
                CaptureMethod::WasapiExclusiveEvent,
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ]
        );
        assert_eq!(
            capture_candidates(CaptureMethod::Auto, &[CaptureMethod::WasapiSharedEvent]),
            vec![
                CaptureMethod::WasapiSharedEvent,
                CaptureMethod::WasapiSharedPolling,
                CaptureMethod::WinMmWaveIn,
            ]
        );
    }

    #[test]
    fn explicit_capture_method_does_not_fall_back() {
        assert_eq!(
            capture_candidates(
                CaptureMethod::WasapiExclusivePolling,
                &[CaptureMethod::WasapiSharedEvent],
            ),
            vec![CaptureMethod::WasapiExclusivePolling]
        );
    }
}

#[cfg(windows)]
pub fn start_microphone_capture_with_auto_priority(
    endpoint_id: impl Into<String>,
    method: CaptureMethod,
    ring: AudioRingBuffer,
    auto_priority: Vec<CaptureMethod>,
) -> Result<CaptureHandle> {
    spawn_endpoint_capture(endpoint_id.into(), false, method, ring, auto_priority)
}

#[cfg(windows)]
pub fn start_microphone_capture(
    endpoint_id: impl Into<String>,
    ring: AudioRingBuffer,
) -> Result<CaptureHandle> {
    start_microphone_capture_with_method(endpoint_id, CaptureMethod::Auto, ring)
}

#[cfg(windows)]
pub fn start_endpoint_loopback(
    endpoint_id: impl Into<String>,
    ring: AudioRingBuffer,
) -> Result<CaptureHandle> {
    spawn_endpoint_capture(
        endpoint_id.into(),
        true,
        CaptureMethod::WasapiSharedEvent,
        ring,
        Vec::new(),
    )
}

#[cfg(not(windows))]
pub fn start_shared_meter(_endpoint_id: impl Into<String>) -> Result<MeterCaptureHandle> {
    Err(BackendError::PlatformUnsupported(
        "WASAPI shared meter requires Windows".into(),
    ))
}

#[cfg(not(windows))]
pub fn start_microphone_capture_with_auto_priority(
    endpoint_id: impl Into<String>,
    method: CaptureMethod,
    ring: AudioRingBuffer,
    _auto_priority: Vec<CaptureMethod>,
) -> Result<CaptureHandle> {
    start_microphone_capture_with_method(endpoint_id, method, ring)
}

#[cfg(not(windows))]
pub fn start_microphone_capture(
    endpoint_id: impl Into<String>,
    ring: AudioRingBuffer,
) -> Result<CaptureHandle> {
    start_microphone_capture_with_method(endpoint_id, CaptureMethod::Auto, ring)
}

#[cfg(not(windows))]
pub fn start_endpoint_loopback(
    _endpoint_id: impl Into<String>,
    _ring: AudioRingBuffer,
) -> Result<CaptureHandle> {
    Err(BackendError::PlatformUnsupported(
        "WASAPI loopback requires Windows".into(),
    ))
}
