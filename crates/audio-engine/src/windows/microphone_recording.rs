use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::audio::graph::RecordingFormat;
use crate::{
    audio::{
        buffer::AudioRingBuffer,
        format::{INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
    },
    error::{BackendError, Result},
    recording::writer::RecorderSink,
    windows::{
        devices::{list_devices, DeviceDirection},
        wasapi_capture::start_microphone_capture,
        worker_status::DeviceWorkerSnapshot,
    },
};

pub const DEFAULT_MICROPHONE_RECORDING_DURATION_MS: u64 = 10_000;
pub const DEFAULT_MICROPHONE_STARTUP_TIMEOUT_MS: u64 = 3_000;
const MAX_MICROPHONE_RECORDING_DURATION_MS: u64 = 24 * 60 * 60 * 1_000;
const CAPTURE_RING_SECONDS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MicrophoneRecordingRequest {
    pub endpoint_id: String,
    pub output_path: PathBuf,
    pub duration_ms: u64,
    pub startup_timeout_ms: u64,
}

impl MicrophoneRecordingRequest {
    pub fn new(
        endpoint_id: impl Into<String>,
        output_path: impl Into<PathBuf>,
        duration_ms: u64,
    ) -> Self {
        Self {
            endpoint_id: endpoint_id.into(),
            output_path: output_path.into(),
            duration_ms,
            startup_timeout_ms: DEFAULT_MICROPHONE_STARTUP_TIMEOUT_MS,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.endpoint_id.trim().is_empty() {
            return Err(BackendError::InvalidOperation(
                "マイクのEndpoint IDを指定してください".into(),
            ));
        }
        if self.duration_ms == 0 || self.duration_ms > MAX_MICROPHONE_RECORDING_DURATION_MS {
            return Err(BackendError::InvalidOperation(format!(
                "録音時間は1 ms以上{} ms以下で指定してください",
                MAX_MICROPHONE_RECORDING_DURATION_MS
            )));
        }
        if self.startup_timeout_ms == 0 || self.startup_timeout_ms > 60_000 {
            return Err(BackendError::InvalidOperation(
                "WASAPI起動待ち時間は1 ms以上60000 ms以下で指定してください".into(),
            ));
        }
        let extension = self
            .output_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("wav") {
            return Err(BackendError::InvalidOperation(
                "マイク録音の出力先には.wavファイルを指定してください".into(),
            ));
        }
        if self.output_path.exists() {
            return Err(BackendError::InvalidOperation(format!(
                "既存ファイルは上書きしません: {}",
                self.output_path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WavRecordingMetrics {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_format: String,
    pub captured_samples: u64,
    pub captured_frames: u64,
    pub audio_duration_ms: u64,
    pub peak: f32,
    pub rms: f32,
    pub non_silent_samples: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MicrophoneRecordingReport {
    pub endpoint_id: String,
    pub friendly_name: String,
    pub output_path: PathBuf,
    pub requested_duration_ms: u64,
    pub elapsed_duration_ms: u64,
    pub worker_status: DeviceWorkerSnapshot,
    pub dropped_samples: u64,
    pub wav: WavRecordingMetrics,
}

/// Records one Windows capture endpoint directly to a 48 kHz Float32 stereo WAV.
///
/// This function is independent from the graph runtime and from every UI. It
/// opens the exact Endpoint ID requested by the caller and never substitutes a
/// default or similarly named microphone.
pub fn record_microphone_to_wav(
    request: &MicrophoneRecordingRequest,
) -> Result<MicrophoneRecordingReport> {
    record_microphone_to_wav_controlled(request, &AtomicBool::new(false), |_, _, _| {})
}

/// 対話収録用。停止要求と現在の Peak/RMS・経過時間を扱う。
pub fn record_microphone_to_wav_controlled(
    request: &MicrophoneRecordingRequest,
    stop: &AtomicBool,
    mut progress: impl FnMut(f32, f32, u64),
) -> Result<MicrophoneRecordingReport> {
    request.validate()?;

    let device = list_devices(DeviceDirection::Capture)?
        .into_iter()
        .find(|device| {
            device
                .endpoint_id
                .eq_ignore_ascii_case(&request.endpoint_id)
        })
        .ok_or_else(|| {
            BackendError::InvalidOperation(format!(
                "指定されたCapture Endpointが見つかりません: {}",
                request.endpoint_id
            ))
        })?;

    let ring = AudioRingBuffer::new(
        CAPTURE_RING_SECONDS * INTERNAL_SAMPLE_RATE as usize * INTERNAL_CHANNELS as usize,
    );
    let capture = start_microphone_capture(&device.endpoint_id, ring.clone())?;
    if let Err(error) =
        capture.wait_until_running(Duration::from_millis(request.startup_timeout_ms))
    {
        capture.stop();
        return Err(error);
    }

    // Exclude initialization packets so the requested interval starts after
    // WASAPI has reached Running.
    ring.clear();
    let writer_ring = AudioRingBuffer::new(
        CAPTURE_RING_SECONDS * INTERNAL_SAMPLE_RATE as usize * INTERNAL_CHANNELS as usize,
    );
    let recorder = match RecorderSink::start_with_ring(
        &request.output_path,
        RecordingFormat::Wav,
        writer_ring.clone(),
    ) {
        Ok(recorder) => recorder,
        Err(error) => {
            capture.stop();
            return Err(error);
        }
    };

    let started = Instant::now();
    let requested = Duration::from_millis(request.duration_ms);
    let mut samples = vec![0.0f32; INTERNAL_SAMPLE_RATE as usize * INTERNAL_CHANNELS as usize];
    let mut transfer_dropped = 0u64;
    while started.elapsed() < requested && !stop.load(Ordering::Relaxed) {
        let count = ring.pop_into(&mut samples);
        if count > 0 {
            transfer_dropped += (count - writer_ring.push_slice(&samples[..count])) as u64;
            let peak = samples[..count].iter().fold(0.0f32, |p, s| p.max(s.abs()));
            let rms = (samples[..count]
                .iter()
                .map(|s| f64::from(*s).powi(2))
                .sum::<f64>()
                / count as f64)
                .sqrt() as f32;
            progress(peak, rms, started.elapsed().as_millis() as u64);
        } else {
            progress(0.0, 0.0, started.elapsed().as_millis() as u64);
        }
        if capture.status().state != crate::windows::worker_status::DeviceWorkerState::Running {
            break;
        }
        let remaining = requested.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(20)));
    }

    let worker_status = capture.status();
    let dropped_samples = capture.dropped_samples() + transfer_dropped;
    capture.stop();
    loop {
        let count = ring.pop_into(&mut samples);
        if count == 0 {
            break;
        }
        writer_ring.push_slice(&samples[..count]);
    }
    if let Err(error) = recorder.stop() {
        let _ = std::fs::remove_file(&request.output_path);
        return Err(error);
    }

    let wav = inspect_recorded_wav(&request.output_path)?;
    if worker_status.state != crate::windows::worker_status::DeviceWorkerState::Running {
        return Err(BackendError::Audio(format!(
            "録音中にマイクが停止しました。部分録音は保持します: {}",
            request.output_path.display()
        )));
    }
    if wav.captured_frames == 0 {
        return Err(BackendError::Audio(format!(
            "WASAPI録音にAudio Frameがありません: {}",
            request.output_path.display()
        )));
    }

    Ok(MicrophoneRecordingReport {
        endpoint_id: device.endpoint_id,
        friendly_name: device.friendly_name,
        output_path: request.output_path.clone(),
        requested_duration_ms: request.duration_ms,
        elapsed_duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        worker_status,
        dropped_samples,
        wav,
    })
}

pub fn inspect_recorded_wav(path: impl AsRef<Path>) -> Result<WavRecordingMetrics> {
    let path = path.as_ref();
    let mut reader =
        hound::WavReader::open(path).map_err(|error| BackendError::Audio(error.to_string()))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(BackendError::Audio(format!(
            "WAV Formatが不正です: {}",
            path.display()
        )));
    }
    if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
        return Err(BackendError::Audio(format!(
            "WAVは32-bit Floatである必要があります: {}",
            path.display()
        )));
    }

    let mut captured_samples = 0u64;
    let mut non_silent_samples = 0u64;
    let mut peak = 0.0f32;
    let mut square_sum = 0.0f64;
    for sample in reader.samples::<f32>() {
        let sample = sample.map_err(|error| BackendError::Audio(error.to_string()))?;
        let absolute = sample.abs();
        peak = peak.max(absolute);
        square_sum += f64::from(sample) * f64::from(sample);
        captured_samples = captured_samples.saturating_add(1);
        if absolute > 0.000_001 {
            non_silent_samples = non_silent_samples.saturating_add(1);
        }
    }
    let captured_frames = captured_samples / u64::from(spec.channels);
    let audio_duration_ms = captured_frames
        .saturating_mul(1_000)
        .checked_div(u64::from(spec.sample_rate))
        .unwrap_or(0);
    let rms = if captured_samples == 0 {
        0.0
    } else {
        (square_sum / captured_samples as f64).sqrt() as f32
    };

    Ok(WavRecordingMetrics {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        bits_per_sample: spec.bits_per_sample,
        sample_format: "float".into(),
        captured_samples,
        captured_frames,
        audio_duration_ms,
        peak,
        rms,
        non_silent_samples,
    })
}
