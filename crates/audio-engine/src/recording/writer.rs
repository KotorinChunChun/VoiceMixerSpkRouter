use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};
use oxiaudio_core::{AudioBuffer as OxiAudioBuffer, ChannelLayout, SampleFormat};
use oxiaudio_encode::FlacStreamingEncoder;

use crate::{
    audio::graph::RecordingFormat,
    audio::{
        buffer::AudioRingBuffer,
        engine::AudioSink,
        format::{AudioBlock, INTERNAL_SAMPLE_RATE},
    },
    error::{BackendError, Result},
};

pub struct RecorderSink {
    ring: AudioRingBuffer,
    stop: Arc<AtomicBool>,
    join: parking_lot::Mutex<Option<JoinHandle<()>>>,
    error: Arc<parking_lot::Mutex<Option<String>>>,
    path: PathBuf,
    format: RecordingFormat,
}

impl RecorderSink {
    pub fn start(
        path: impl AsRef<Path>,
        format: RecordingFormat,
        buffer_seconds: usize,
    ) -> Result<Self> {
        let ring = AudioRingBuffer::new(buffer_seconds.max(1) * INTERNAL_SAMPLE_RATE as usize * 2);
        Self::start_with_ring(path, format, ring)
    }

    /// Starts a recorder that drains a caller-provided ring buffer.
    ///
    /// This is used by UI-independent device probes so a WASAPI producer can
    /// write directly to the same bounded queue as the recording worker.
    pub fn start_with_ring(
        path: impl AsRef<Path>,
        format: RecordingFormat,
        ring: AudioRingBuffer,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_extension(&path, format)?;

        let reader_ring = ring.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let error = Arc::new(parking_lot::Mutex::new(None));
        let error_thread = error.clone();
        let worker_path = path.clone();
        let join = thread::Builder::new()
            .name("audio-recorder-writer".into())
            .spawn(move || {
                if let Err(err) = run_writer(worker_path, format, reader_ring, stop_thread) {
                    *error_thread.lock() = Some(err.to_string());
                }
            })
            .map_err(BackendError::Io)?;
        Ok(Self {
            ring,
            stop,
            join: parking_lot::Mutex::new(Some(join)),
            error,
            path,
            format,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn format(&self) -> RecordingFormat {
        self.format
    }

    pub fn stop(&self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.lock().take() {
            join.join().map_err(|_| {
                BackendError::Audio("recording writer thread terminated unexpectedly".into())
            })?;
        }
        if let Some(message) = self.error.lock().clone() {
            return Err(BackendError::Audio(message));
        }
        Ok(())
    }
}

fn validate_extension(path: &Path, format: RecordingFormat) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let expected = match format {
        RecordingFormat::Wav => "wav",
        RecordingFormat::Flac => "flac",
        RecordingFormat::Mp3 => "mp3",
    };
    if ext != expected {
        return Err(BackendError::InvalidOperation(format!(
            "recorder format is {expected} but output path is {}",
            path.display()
        )));
    }
    Ok(())
}

fn run_writer(
    path: PathBuf,
    format: RecordingFormat,
    ring: AudioRingBuffer,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    match format {
        RecordingFormat::Wav => write_wav(path, ring, stop),
        RecordingFormat::Flac => write_flac(path, ring, stop),
        RecordingFormat::Mp3 => write_mp3(path, ring, stop),
    }
}

fn write_wav(path: PathBuf, ring: AudioRingBuffer, stop: Arc<AtomicBool>) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: INTERNAL_SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer =
        hound::WavWriter::create(&path, spec).map_err(|e| BackendError::Audio(e.to_string()))?;
    drain_ring(&ring, &stop, |samples| {
        for sample in samples {
            writer
                .write_sample(*sample)
                .map_err(|e| BackendError::Audio(e.to_string()))?;
        }
        Ok(())
    })?;
    writer
        .finalize()
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    Ok(())
}

fn write_flac(path: PathBuf, ring: AudioRingBuffer, stop: Arc<AtomicBool>) -> Result<()> {
    let file = File::create(&path)?;
    let mut encoder = FlacStreamingEncoder::with_bits_per_sample(
        file,
        INTERNAL_SAMPLE_RATE,
        ChannelLayout::Stereo,
        5,
        24,
    )
    .map_err(|e| BackendError::Audio(format!("FLAC encoder: {e}")))?;

    drain_ring(&ring, &stop, |samples| {
        // Ensure only complete stereo frames are sent to the encoder.
        let n = samples.len() - (samples.len() % 2);
        if n == 0 {
            return Ok(());
        }
        let chunk = OxiAudioBuffer {
            samples: samples[..n].to_vec(),
            sample_rate: INTERNAL_SAMPLE_RATE,
            channels: ChannelLayout::Stereo,
            format: SampleFormat::F32,
        };
        encoder
            .encode_chunk(&chunk)
            .map_err(|e| BackendError::Audio(format!("FLAC encode: {e}")))
    })?;
    encoder
        .finalize()
        .map_err(|e| BackendError::Audio(format!("FLAC finalize: {e}")))?;
    Ok(())
}

fn write_mp3(path: PathBuf, ring: AudioRingBuffer, stop: Arc<AtomicBool>) -> Result<()> {
    let mut encoder = Builder::new()
        .ok_or_else(|| BackendError::Audio("MP3 encoder allocation failed".into()))?
        .with_num_channels(2)
        .map_err(|e| BackendError::Audio(format!("MP3 channels: {e}")))?
        .with_sample_rate(INTERNAL_SAMPLE_RATE)
        .map_err(|e| BackendError::Audio(format!("MP3 sample rate: {e}")))?
        .with_brate(Bitrate::Kbps192)
        .map_err(|e| BackendError::Audio(format!("MP3 bitrate: {e}")))?
        .with_quality(Quality::Best)
        .map_err(|e| BackendError::Audio(format!("MP3 quality: {e}")))?
        .build()
        .map_err(|e| BackendError::Audio(format!("MP3 build: {e}")))?;
    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);
    let mut encoded = Vec::<u8>::new();

    drain_ring(&ring, &stop, |samples| {
        let n = samples.len() - (samples.len() % 2);
        if n == 0 {
            return Ok(());
        }
        encoded.clear();
        encoded.reserve(mp3lame_encoder::max_required_buffer_size(n / 2));
        encoder
            .encode_to_vec(InterleavedPcm(&samples[..n]), &mut encoded)
            .map_err(|e| BackendError::Audio(format!("MP3 encode: {e}")))?;
        writer.write_all(&encoded)?;
        Ok(())
    })?;

    encoded.clear();
    encoded.reserve(7200);
    encoder
        .flush_to_vec::<FlushNoGap>(&mut encoded)
        .map_err(|e| BackendError::Audio(format!("MP3 flush: {e}")))?;
    writer.write_all(&encoded)?;
    writer.flush()?;
    Ok(())
}

fn drain_ring<F>(ring: &AudioRingBuffer, stop: &AtomicBool, mut consume: F) -> Result<()>
where
    F: FnMut(&[f32]) -> Result<()>,
{
    let mut scratch = vec![0.0f32; 8192];
    loop {
        let n = ring.pop_into(&mut scratch);
        if n > 0 {
            consume(&scratch[..n])?;
        }
        if stop.load(Ordering::Acquire) && ring.len_samples() == 0 {
            break;
        }
        if n == 0 {
            thread::sleep(Duration::from_millis(4));
        }
    }
    Ok(())
}

impl AudioSink for RecorderSink {
    fn write(&self, block: &AudioBlock) -> Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let written = self.ring.push_slice(&block.samples);
        if written < block.samples.len() {
            return Err(BackendError::Audio(format!(
                "recording ring overflow: dropped {} samples",
                block.samples.len() - written
            )));
        }
        Ok(())
    }
}

impl Drop for RecorderSink {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
