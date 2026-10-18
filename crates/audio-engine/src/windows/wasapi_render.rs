use crate::{
    audio::{
        buffer::AudioRingBuffer,
        format::{INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        level_probe::LevelProbe,
    },
    error::{BackendError, Result},
    windows::device_format::DeviceFormat,
    windows::worker_status::{DeviceWorkerSnapshot, DeviceWorkerStatus},
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct RenderHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    status: DeviceWorkerStatus,
    padding_frames: Arc<AtomicU32>,
    level_probe: Arc<LevelProbe>,
}
impl RenderHandle {
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
    pub fn padding_frames(&self) -> Arc<AtomicU32> {
        self.padding_frames.clone()
    }
    pub fn level_probe(&self) -> Arc<LevelProbe> {
        self.level_probe.clone()
    }
}
impl Drop for RenderHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(windows)]
pub fn start_render(endpoint_id: impl Into<String>, ring: AudioRingBuffer) -> Result<RenderHandle> {
    start_render_with_exclusive(endpoint_id, ring, false)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenderProbeClass {
    Ok,
    FormatUnsupported,
    DeviceInUse,
    DeviceNotFound,
    Other,
}

pub fn classify_render_error(message: &str) -> RenderProbeClass {
    let lower = message.to_ascii_lowercase();
    if lower.contains("get_device") || lower.contains("not found") || lower.contains("no matching")
    {
        RenderProbeClass::DeviceNotFound
    } else if lower.contains("device_in_use")
        || lower.contains("in use")
        || lower.contains("0x8889000a")
        || lower.contains("audclnt_e_device_in_use")
    {
        RenderProbeClass::DeviceInUse
    } else if lower.contains("unsupported")
        || lower.contains("format")
        || lower.contains("0x88890008")
        || lower.contains("is_supported")
    {
        RenderProbeClass::FormatUnsupported
    } else {
        RenderProbeClass::Other
    }
}

#[cfg(windows)]
pub fn start_render_with_exclusive(
    endpoint_id: impl Into<String>,
    ring: AudioRingBuffer,
    exclusive: bool,
) -> Result<RenderHandle> {
    let endpoint_id = endpoint_id.into();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let status = DeviceWorkerStatus::new();
    let worker_status = status.clone();
    let padding_frames = Arc::new(AtomicU32::new(0));
    let worker_padding = padding_frames.clone();
    let level_probe = Arc::new(LevelProbe::default());
    let worker_probe = level_probe.clone();
    let join = thread::Builder::new()
        .name("wasapi-render".into())
        .spawn(move || {
            use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
            let _ = wasapi::initialize_mta();
            let mut prefer_exclusive = exclusive;
            while !worker_stop.load(Ordering::Acquire) {
                worker_probe.restart();
                let run = || -> std::result::Result<(), String> {
                    let en = DeviceEnumerator::new()
                        .map_err(|e| format!("DeviceEnumerator::new: {e}"))?;
                    let device = en
                        .get_device(&endpoint_id)
                        .map_err(|e| format!("get_device: {e}"))?;
                    let mut client = device
                        .get_iaudioclient()
                        .map_err(|e| format!("get_iaudioclient: {e}"))?;
                    // 共有モードの既定再生デバイスは、機器ごとにMixFormatの
                    // サンプルレート・チャンネル数・有効ビット数が異なる。
                    // Backend内部の形式を共有モードへ渡し、WASAPIの自動変換に
                    // 任せることで、既定デバイス側の形式差による無音を避ける。
                    let mix_fmt = client
                        .get_mixformat()
                        .map_err(|e| format!("get_mixformat: {e}"))?;
                    let (fmt, mode) = if prefer_exclusive {
                        let exclusive_fmt = exclusive_render_format(&mut client, &mix_fmt)?;
                        let (default_period, _) = client
                            .get_device_period()
                            .map_err(|e| format!("get_device_period: {e}"))?;
                        let period = client
                            .calculate_aligned_period_near(default_period, Some(128), &exclusive_fmt)
                            .map_err(|e| format!("calculate_aligned_period_near: {e}"))?;
                        (
                            exclusive_fmt,
                            StreamMode::EventsExclusive { period_hns: period },
                        )
                    } else {
                        let shared = WaveFormat::new(
                            32,
                            32,
                            &SampleType::Float,
                            INTERNAL_SAMPLE_RATE as usize,
                            INTERNAL_CHANNELS as usize,
                            None,
                        );
                        let (default_period, _) = client
                            .get_device_period()
                            .map_err(|e| format!("get_device_period: {e}"))?;
                        (
                            shared,
                            StreamMode::EventsShared {
                                autoconvert: true,
                                buffer_duration_hns: default_period,
                            },
                        )
                    };
                    let device_format = DeviceFormat::from_wave_format(&fmt)
                        .map_err(|e| format!("device format: {e}"))?;
                    client
                        .initialize_client(&fmt, &Direction::Render, &mode)
                        .map_err(|e| {
                            format!(
                                "initialize_client: {e}; endpoint={endpoint_id}; requested={fmt:?}; mix={mix_fmt:?}; exclusive={prefer_exclusive}; mode={mode:?}"
                            )
                        })?;
                    let event = client
                        .set_get_eventhandle()
                        .map_err(|e| format!("set_get_eventhandle: {e}"))?;
                    let render = client
                        .get_audiorenderclient()
                        .map_err(|e| format!("get_audiorenderclient: {e}"))?;
                    let max_frames = client
                        .get_buffer_size()
                        .map_err(|e| format!("get_buffer_size: {e}"))?
                        as usize;
                    let maximum_internal_frames = ((max_frames as u64
                        * INTERNAL_SAMPLE_RATE as u64)
                        .div_ceil(device_format.sample_rate() as u64)
                        + 1) as usize;
                    let mut samples =
                        vec![0.0f32; maximum_internal_frames * INTERNAL_CHANNELS as usize];
                    let mut bytes = vec![
                        0u8;
                        device_format
                            .bytes_for_frames(max_frames)
                            .map_err(|e| e.to_string())?
                    ];
                    if let Ok(space) = client.get_available_space_in_frames() {
                        let frames = space as usize;
                        let byte_count = device_format
                            .bytes_for_frames(frames)
                            .map_err(|e| e.to_string())?;
                        render
                            .write_to_device(frames, &bytes[..byte_count], None)
                            .map_err(|e| e.to_string())?;
                    }
                    client
                        .start_stream()
                        .map_err(|e| format!("start_stream: {e}"))?;
                    worker_status.running();
                    while !worker_stop.load(Ordering::Acquire) {
                        if event.wait_for_event(100).is_err() {
                            continue;
                        }
                        let frames = client
                            .get_available_space_in_frames()
                            .map_err(|e| e.to_string())?
                            as usize;
                        let padding = client.get_current_padding().ok();
                        if let Some(padding) = padding {
                            worker_padding.store(padding, Ordering::Relaxed);
                        }
                        if frames == 0 {
                            continue;
                        }
                        let internal_frames = if device_format.sample_rate() == INTERNAL_SAMPLE_RATE
                        {
                            frames
                        } else {
                            ((frames as u64 * INTERNAL_SAMPLE_RATE as u64)
                                .div_ceil(device_format.sample_rate() as u64))
                                as usize
                        };
                        let needed = internal_frames * INTERNAL_CHANNELS as usize;
                        samples[..needed].fill(0.0);
                        let _ = ring.pop_into(&mut samples[..needed]);
                        let byte_count = device_format
                            .bytes_for_frames(frames)
                            .map_err(|e| e.to_string())?;
                        device_format
                            .encode_from_internal(
                                &samples[..needed],
                                frames,
                                &mut bytes[..byte_count],
                            )
                            .map_err(|e| e.to_string())?;
                        render
                            .write_to_device(frames, &bytes[..byte_count], None)
                            .map_err(|e| e.to_string())?;
                        // アプリの再生リングを通過したPCMを観測する。デバイス内の
                        // 先行フレームの再生待ちを加え、RVC直後の時刻で代用しない。
                        if let Some(padding) = padding {
                            let at = crate::audio::clock::now_100ns().saturating_add(
                                crate::audio::clock::frames_to_100ns(
                                    padding as usize,
                                    device_format.sample_rate(),
                                ),
                            );
                            worker_probe.observe(&samples[..needed], at, INTERNAL_SAMPLE_RATE, 2);
                        }
                    }
                    let _ = client.stop_stream();
                    Ok(())
                };
                match run() {
                    Ok(()) => break,
                    Err(error) if !worker_stop.load(Ordering::Acquire) => {
                        if prefer_exclusive {
                            prefer_exclusive = false;
                            worker_status.reconnecting(format!(
                                "exclusive render fallback to shared: {error}"
                            ));
                        } else {
                            worker_status.reconnecting(error);
                        }
                    }
                    Err(_) => break,
                }
                thread::sleep(Duration::from_millis(500));
            }
            worker_status.stopped();
            wasapi::deinitialize();
        })
        .map_err(BackendError::Io)?;
    Ok(RenderHandle {
        stop,
        join: Some(join),
        status,
        padding_frames,
        level_probe,
    })
}

#[cfg(not(windows))]
pub fn start_render(
    _endpoint_id: impl Into<String>,
    _ring: AudioRingBuffer,
) -> Result<RenderHandle> {
    Err(BackendError::PlatformUnsupported(
        "WASAPI render requires Windows".into(),
    ))
}

#[cfg(not(windows))]
pub fn start_render_with_exclusive(
    endpoint_id: impl Into<String>,
    ring: AudioRingBuffer,
    _exclusive: bool,
) -> Result<RenderHandle> {
    start_render(endpoint_id, ring)
}

#[cfg(windows)]
fn exclusive_render_format(
    client: &mut wasapi::AudioClient,
    mix_fmt: &wasapi::WaveFormat,
) -> std::result::Result<wasapi::WaveFormat, String> {
    use wasapi::{SampleType, WaveFormat};
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
                WaveFormat::new(16, 16, &SampleType::Int, sample_rate, channel_count, None),
            ]);
        }
    }
    let mut errors = Vec::new();
    for candidate in candidates {
        match client.is_supported_exclusive_with_quirks(&candidate) {
            Ok(format) => return Ok(format),
            Err(error) => errors.push(format!("{candidate:?}: {error}")),
        }
    }
    Err(format!(
        "exclusive render format negotiation failed; mix={mix_fmt:?}; tried={}",
        errors.join(" | ")
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderProbeResult {
    pub ok: bool,
    pub class: RenderProbeClass,
    pub message: String,
    pub exclusive: bool,
    pub endpoint_id: String,
}

#[cfg(windows)]
pub fn probe_exclusive_render(
    endpoint_id: impl Into<String>,
    duration: Duration,
) -> RenderProbeResult {
    let endpoint_id = endpoint_id.into();
    match probe_exclusive_render_inner(&endpoint_id, duration) {
        Ok(()) => RenderProbeResult {
            ok: true,
            class: RenderProbeClass::Ok,
            message: "exclusive render initialized".into(),
            exclusive: true,
            endpoint_id,
        },
        Err(message) => RenderProbeResult {
            ok: false,
            class: classify_render_error(&message),
            message,
            exclusive: true,
            endpoint_id,
        },
    }
}

#[cfg(windows)]
fn probe_exclusive_render_inner(
    endpoint_id: &str,
    duration: Duration,
) -> std::result::Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, StreamMode};
    let _ = wasapi::initialize_mta();
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
    let fmt = exclusive_render_format(&mut client, &mix_fmt)?;
    let (default_period, _) = client
        .get_device_period()
        .map_err(|e| format!("get_device_period: {e}"))?;
    let period = client
        .calculate_aligned_period_near(default_period, Some(128), &fmt)
        .map_err(|e| format!("calculate_aligned_period_near: {e}"))?;
    let mode = StreamMode::EventsExclusive { period_hns: period };
    client
        .initialize_client(&fmt, &Direction::Render, &mode)
        .map_err(|e| format!("initialize_client: {e}"))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|e| format!("set_get_eventhandle: {e}"))?;
    let render = client
        .get_audiorenderclient()
        .map_err(|e| format!("get_audiorenderclient: {e}"))?;
    let device_format =
        DeviceFormat::from_wave_format(&fmt).map_err(|e| format!("device format: {e}"))?;
    client
        .start_stream()
        .map_err(|e| format!("start_stream: {e}"))?;
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        let _ = event.wait_for_event(50);
        if let Ok(frames) = client.get_available_space_in_frames() {
            let frames = frames as usize;
            if frames == 0 {
                continue;
            }
            let byte_count = device_format
                .bytes_for_frames(frames)
                .map_err(|e| e.to_string())?;
            let bytes = vec![0u8; byte_count];
            render
                .write_to_device(frames, &bytes, None)
                .map_err(|e| e.to_string())?;
        }
    }
    let _ = client.stop_stream();
    wasapi::deinitialize();
    Ok(())
}

#[cfg(not(windows))]
pub fn probe_exclusive_render(
    endpoint_id: impl Into<String>,
    _duration: Duration,
) -> RenderProbeResult {
    RenderProbeResult {
        ok: false,
        class: RenderProbeClass::Other,
        message: "WASAPI render requires Windows".into(),
        exclusive: true,
        endpoint_id: endpoint_id.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_render_error, RenderProbeClass};

    #[test]
    fn classifies_device_in_use_and_format_errors() {
        assert_eq!(
            classify_render_error("AUDCLNT_E_DEVICE_IN_USE"),
            RenderProbeClass::DeviceInUse
        );
        assert_eq!(
            classify_render_error("initialize_client: unsupported format 0x88890008"),
            RenderProbeClass::FormatUnsupported
        );
        assert_eq!(
            classify_render_error("get_device: not found"),
            RenderProbeClass::DeviceNotFound
        );
    }
}
