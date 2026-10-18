use crate::{
    audio::{
        format::{INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        resampler::resample_interleaved_linear,
    },
    error::{BackendError, Result},
};
use wasapi::{SampleType, WaveFormat};

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeviceFormat {
    sample_type: SampleType,
    sample_rate: u32,
    channels: u16,
    bytes_per_sample: usize,
    block_align: usize,
    valid_bits: u16,
}

impl DeviceFormat {
    pub(crate) fn from_wave_format(format: &WaveFormat) -> Result<Self> {
        let channels = format.get_nchannels();
        let block_align = format.get_blockalign() as usize;
        if channels == 0 || block_align == 0 || block_align % channels as usize != 0 {
            return Err(BackendError::Audio(format!(
                "WASAPI MixFormatのBlockAlignが不正です: {format:?}"
            )));
        }
        let bytes_per_sample = block_align / channels as usize;
        let store_bits = format.get_bitspersample();
        if store_bits as usize > bytes_per_sample * 8 {
            return Err(BackendError::Audio(format!(
                "WASAPI MixFormatのサンプル幅が不正です: {format:?}"
            )));
        }
        let sample_type = format
            .get_subformat()
            .map_err(|error| BackendError::Audio(format!("WASAPI MixFormat: {error}")))?;
        match (sample_type, bytes_per_sample) {
            (SampleType::Float, 4 | 8) | (SampleType::Int, 1..=4) => {}
            _ => {
                return Err(BackendError::Audio(format!(
                    "未対応のWASAPI MixFormatです: {format:?}"
                )))
            }
        }
        Ok(Self {
            sample_type,
            sample_rate: format.get_samplespersec(),
            channels,
            bytes_per_sample,
            block_align,
            valid_bits: format.get_validbitspersample().max(1),
        })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn bytes_for_frames(&self, frames: usize) -> Result<usize> {
        frames
            .checked_mul(self.block_align)
            .ok_or_else(|| BackendError::Audio("WASAPIバッファ長が上限を超えました".into()))
    }

    pub(crate) fn decode_to_internal(
        &self,
        bytes: &[u8],
        frames: usize,
        silent: bool,
    ) -> Result<Vec<f32>> {
        let required = self.bytes_for_frames(frames)?;
        if bytes.len() < required {
            return Err(BackendError::Audio(format!(
                "WASAPI Captureデータが不足しています: expected={required}, actual={}",
                bytes.len()
            )));
        }
        let sample_count = frames * self.channels as usize;
        let mut decoded = vec![0.0f32; sample_count];
        if !silent {
            for (destination, source) in decoded
                .iter_mut()
                .zip(bytes[..required].chunks_exact(self.bytes_per_sample))
            {
                *destination = self.decode_sample(source)?;
            }
        }
        if self.sample_rate == INTERNAL_SAMPLE_RATE && self.channels == INTERNAL_CHANNELS {
            return Ok(decoded);
        }
        Ok(resample_interleaved_linear(
            &decoded,
            self.sample_rate,
            self.channels,
            INTERNAL_SAMPLE_RATE,
            INTERNAL_CHANNELS,
            1.0,
        )
        .samples)
    }

    pub(crate) fn encode_from_internal(
        &self,
        internal: &[f32],
        frames: usize,
        destination: &mut [u8],
    ) -> Result<()> {
        let required = self.bytes_for_frames(frames)?;
        if destination.len() < required {
            return Err(BackendError::Audio(format!(
                "WASAPI Renderバッファが不足しています: expected={required}, actual={}",
                destination.len()
            )));
        }
        destination[..required].fill(0);
        let converted;
        let samples =
            if self.sample_rate == INTERNAL_SAMPLE_RATE && self.channels == INTERNAL_CHANNELS {
                internal
            } else {
                converted = resample_interleaved_linear(
                    internal,
                    INTERNAL_SAMPLE_RATE,
                    INTERNAL_CHANNELS,
                    self.sample_rate,
                    self.channels,
                    1.0,
                )
                .samples;
                &converted
            };
        let destination_samples = frames * self.channels as usize;
        for (source, target) in samples
            .iter()
            .take(destination_samples)
            .zip(destination[..required].chunks_exact_mut(self.bytes_per_sample))
        {
            self.encode_sample(*source, target)?;
        }
        Ok(())
    }

    fn decode_sample(&self, bytes: &[u8]) -> Result<f32> {
        match (self.sample_type, self.bytes_per_sample) {
            (SampleType::Float, 4) => Ok(f32::from_le_bytes(bytes.try_into().unwrap())),
            (SampleType::Float, 8) => Ok(f64::from_le_bytes(bytes.try_into().unwrap()) as f32),
            (SampleType::Int, 1) => Ok((bytes[0] as f32 - 128.0) / 128.0),
            (SampleType::Int, 2) => {
                Ok(i16::from_le_bytes(bytes.try_into().unwrap()) as f32 / 32_768.0)
            }
            (SampleType::Int, 3) => {
                let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
                let signed = (raw << 8) >> 8;
                Ok(signed as f32 / 8_388_608.0)
            }
            (SampleType::Int, 4) => {
                Ok(i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0)
            }
            _ => Err(BackendError::Audio("未対応のWASAPIサンプル形式です".into())),
        }
    }

    fn encode_sample(&self, sample: f32, bytes: &mut [u8]) -> Result<()> {
        let sample = sample.clamp(-1.0, 1.0);
        match (self.sample_type, self.bytes_per_sample) {
            (SampleType::Float, 4) => bytes.copy_from_slice(&sample.to_le_bytes()),
            (SampleType::Float, 8) => bytes.copy_from_slice(&(sample as f64).to_le_bytes()),
            (SampleType::Int, 1) => {
                bytes[0] = (sample * 128.0 + 128.0).round().clamp(0.0, 255.0) as u8;
            }
            (SampleType::Int, width @ 2..=4) => {
                let store_bits = (width * 8) as u16;
                let scale = (1_i64 << (store_bits - 1)) as f64;
                let maximum = scale - 1.0;
                let mut value = (sample as f64 * scale).round().clamp(-scale, maximum) as i64;
                let unused_bits = store_bits.saturating_sub(self.valid_bits.min(store_bits));
                if unused_bits > 0 {
                    value = (value >> unused_bits) << unused_bits;
                }
                let encoded = value.to_le_bytes();
                bytes.copy_from_slice(&encoded[..width]);
            }
            _ => return Err(BackendError::Audio("未対応のWASAPIサンプル形式です".into())),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float32_stereo_round_trip_keeps_internal_samples() {
        let wave = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let format = DeviceFormat::from_wave_format(&wave).unwrap();
        let source = [-0.75, 0.5, 0.25, -0.125];
        let mut bytes = vec![0; format.bytes_for_frames(2).unwrap()];
        format.encode_from_internal(&source, 2, &mut bytes).unwrap();
        let decoded = format.decode_to_internal(&bytes, 2, false).unwrap();
        assert_eq!(decoded, source);
    }

    #[test]
    fn integer_mono_device_is_converted_to_internal_stereo() {
        let wave = WaveFormat::new(16, 16, &SampleType::Int, 24_000, 1, None);
        let format = DeviceFormat::from_wave_format(&wave).unwrap();
        let bytes = [0x00, 0x40, 0x00, 0xC0];
        let decoded = format.decode_to_internal(&bytes, 2, false).unwrap();
        assert_eq!(decoded.len(), 8);
        assert!((decoded[0] - 0.5).abs() < 0.0001);
        assert_eq!(decoded[0], decoded[1]);
        assert!((decoded[6] + 0.5).abs() < 0.0001);
        assert_eq!(decoded[6], decoded[7]);
    }
}
