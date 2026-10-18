use crate::{
    audio::{format::INTERNAL_SAMPLE_RATE, graph::RecordingFormat},
    error::{BackendError, Result},
    recording::codecs::{decode_audio_file, encode_audio_file},
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TrimRange {
    pub start_seconds: f64,
    pub end_seconds: f64,
}

/// Non-destructive edit is represented by `TrimRange`; this function renders a
/// new WAV/FLAC/MP3 file only when explicitly requested.
pub fn export_trimmed(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    range: TrimRange,
) -> Result<()> {
    if !range.start_seconds.is_finite()
        || !range.end_seconds.is_finite()
        || range.start_seconds < 0.0
        || range.end_seconds <= range.start_seconds
    {
        return Err(BackendError::InvalidOperation("invalid trim range".into()));
    }
    let decoded = decode_audio_file(input)?;
    let total_frames = decoded.frames();
    let start_frame = (range.start_seconds * INTERNAL_SAMPLE_RATE as f64).floor() as usize;
    let end_frame = (range.end_seconds * INTERNAL_SAMPLE_RATE as f64).ceil() as usize;
    if start_frame >= total_frames {
        return Err(BackendError::InvalidOperation(
            "trim start is beyond end of recording".into(),
        ));
    }
    let start = start_frame * 2;
    let end = end_frame.min(total_frames) * 2;
    let format = RecordingFormat::from_path(output.as_ref()).ok_or_else(|| {
        BackendError::InvalidOperation("trim output must use .wav, .flac or .mp3".into())
    })?;
    encode_audio_file(output, format, &decoded.samples[start..end])
}

/// Legacy wrapper retained for existing frontend calls.
pub fn export_trimmed_wav(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    range: TrimRange,
) -> Result<()> {
    export_trimmed(input, output, range)
}
