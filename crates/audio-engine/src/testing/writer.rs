use std::path::Path;

use oxiaudio_core::ChannelLayout;

use crate::error::{BackendError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFileFormat {
    Wav,
    Flac,
    Mp3,
}

impl AudioFileFormat {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "wav" => Some(Self::Wav),
            "flac" => Some(Self::Flac),
            "mp3" => Some(Self::Mp3),
            _ => None,
        }
    }
}

/// Write interleaved PCM using the requested sample rate and channel count.
/// Product `encode_audio_file` stays 48 kHz stereo; this writer is test-only.
pub fn write_audio(
    path: impl AsRef<Path>,
    format: AudioFileFormat,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match channels {
        1 | 2 => {}
        other => {
            return Err(BackendError::InvalidOperation(format!(
                "testing writer supports 1 or 2 channels, got {other}"
            )));
        }
    }
    match format {
        AudioFileFormat::Wav => write_wav(path, samples, sample_rate, channels),
        AudioFileFormat::Flac => write_flac(path, samples, sample_rate, channels),
        AudioFileFormat::Mp3 => write_mp3(path, samples, sample_rate, channels),
    }
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| BackendError::Audio(e.to_string()))?;
    for &sample in samples {
        writer
            .write_sample(sample)
            .map_err(|e| BackendError::Audio(e.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|e| BackendError::Audio(e.to_string()))
}

fn write_flac(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    use oxiaudio_core::{AudioBuffer as OxiAudioBuffer, SampleFormat};
    use oxiaudio_encode::{encode_flac_with_config, FlacBitDepth, FlacConfig};
    let buf = OxiAudioBuffer {
        samples: samples.to_vec(),
        sample_rate,
        channels: if channels == 1 {
            ChannelLayout::Mono
        } else {
            ChannelLayout::Stereo
        },
        format: SampleFormat::F32,
    };
    let mut file = std::fs::File::create(path)?;
    let cfg = FlacConfig {
        compression: 5,
        bit_depth: FlacBitDepth::I16,
    };
    encode_flac_with_config(&buf, &mut file, &cfg)
        .map_err(|e| BackendError::Audio(format!("FLAC encode: {e}")))
}

fn write_mp3(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};
    use std::io::Write;
    let mut encoder = Builder::new()
        .ok_or_else(|| BackendError::Audio("MP3 encoder allocation failed".into()))?
        .with_num_channels(channels as u8)
        .map_err(|e| BackendError::Audio(format!("MP3 channels: {e}")))?
        .with_sample_rate(sample_rate)
        .map_err(|e| BackendError::Audio(format!("MP3 sample rate: {e}")))?
        .with_brate(Bitrate::Kbps192)
        .map_err(|e| BackendError::Audio(format!("MP3 bitrate: {e}")))?
        .with_quality(Quality::Best)
        .map_err(|e| BackendError::Audio(format!("MP3 quality: {e}")))?
        .build()
        .map_err(|e| BackendError::Audio(format!("MP3 build: {e}")))?;
    let frame = usize::from(channels.max(1));
    let n = samples.len() - (samples.len() % frame);
    let mut encoded =
        Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(n / frame) + 7200);
    encoder
        .encode_to_vec(InterleavedPcm(&samples[..n]), &mut encoded)
        .map_err(|e| BackendError::Audio(format!("MP3 encode: {e}")))?;
    encoder
        .flush_to_vec::<FlushNoGap>(&mut encoded)
        .map_err(|e| BackendError::Audio(format!("MP3 flush: {e}")))?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(&encoded)?;
    Ok(())
}
