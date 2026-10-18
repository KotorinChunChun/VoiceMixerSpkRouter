use std::path::Path;

use oxiaudio_core::ChannelLayout;

use crate::{
    audio::{format::AudioBlock, resampler::to_internal},
    error::{BackendError, Result},
};

/// Decode one of the supported user-facing audio formats (WAV/FLAC/MP3) and
/// normalize it to the graph format (48 kHz Float32 stereo). Decoding runs on a
/// worker in all realtime call sites.
pub fn decode_audio_file(path: impl AsRef<Path>) -> Result<AudioBlock> {
    let path = path.as_ref();
    ensure_supported_extension(path)?;
    let decoded = oxiaudio_decode::decode_file(path)
        .map_err(|e| BackendError::Audio(format!("decode {}: {e}", path.display())))?;

    match decoded.channels {
        ChannelLayout::Mono => Ok(to_internal(&decoded.samples, decoded.sample_rate, 1)),
        ChannelLayout::Stereo => Ok(to_internal(&decoded.samples, decoded.sample_rate, 2)),
        _ => {
            // The initial product contract is mono/stereo internally. If an MP3,
            // WAV or FLAC contains more channels, downmix deterministically before
            // entering the graph rather than dropping arbitrary channels.
            let mono = oxiaudio_core::downmix_to_mono(&decoded);
            Ok(to_internal(&mono.samples, mono.sample_rate, 1))
        }
    }
}

pub fn ensure_supported_extension(path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "wav" | "flac" | "mp3" => Ok(()),
        _ => Err(BackendError::InvalidOperation(format!(
            "unsupported audio format '{}'; supported formats are WAV, FLAC and MP3",
            if ext.is_empty() { "(none)" } else { &ext }
        ))),
    }
}

pub fn encode_audio_file(
    path: impl AsRef<Path>,
    format: crate::audio::graph::RecordingFormat,
    samples: &[f32],
) -> Result<()> {
    use crate::audio::graph::RecordingFormat;
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match format {
        RecordingFormat::Wav => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: crate::audio::format::INTERNAL_SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut writer = hound::WavWriter::create(path, spec)
                .map_err(|e| BackendError::Audio(e.to_string()))?;
            for &s in samples {
                writer
                    .write_sample(s)
                    .map_err(|e| BackendError::Audio(e.to_string()))?;
            }
            writer
                .finalize()
                .map_err(|e| BackendError::Audio(e.to_string()))?;
            Ok(())
        }
        RecordingFormat::Flac => {
            use oxiaudio_core::{AudioBuffer as OxiAudioBuffer, ChannelLayout, SampleFormat};
            use oxiaudio_encode::{encode_flac_with_config, FlacBitDepth, FlacConfig};
            let buf = OxiAudioBuffer {
                samples: samples.to_vec(),
                sample_rate: crate::audio::format::INTERNAL_SAMPLE_RATE,
                channels: ChannelLayout::Stereo,
                format: SampleFormat::F32,
            };
            let mut file = std::fs::File::create(path)?;
            let cfg = FlacConfig {
                compression: 5,
                bit_depth: FlacBitDepth::I24,
            };
            encode_flac_with_config(&buf, &mut file, &cfg)
                .map_err(|e| BackendError::Audio(format!("FLAC encode: {e}")))
        }
        RecordingFormat::Mp3 => {
            use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};
            use std::io::Write;
            let mut encoder = Builder::new()
                .ok_or_else(|| BackendError::Audio("MP3 encoder allocation failed".into()))?
                .with_num_channels(2)
                .map_err(|e| BackendError::Audio(format!("MP3 channels: {e}")))?
                .with_sample_rate(crate::audio::format::INTERNAL_SAMPLE_RATE)
                .map_err(|e| BackendError::Audio(format!("MP3 sample rate: {e}")))?
                .with_brate(Bitrate::Kbps192)
                .map_err(|e| BackendError::Audio(format!("MP3 bitrate: {e}")))?
                .with_quality(Quality::Best)
                .map_err(|e| BackendError::Audio(format!("MP3 quality: {e}")))?
                .build()
                .map_err(|e| BackendError::Audio(format!("MP3 build: {e}")))?;
            let n = samples.len() - (samples.len() % 2);
            let mut encoded =
                Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(n / 2) + 7200);
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
    }
}
