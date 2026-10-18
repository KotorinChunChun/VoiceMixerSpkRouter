use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use crate::{
    audio::{
        buffer::AudioRingBuffer,
        engine::AudioSource,
        format::{AudioBlock, INTERNAL_SAMPLE_RATE},
    },
    error::{BackendError, Result},
    recording::codecs::{decode_audio_file, ensure_supported_extension},
};

const NO_SEEK: u64 = u64::MAX;

/// File decoder/player for WAV, FLAC and MP3. Decode and seek work lives on a
/// worker thread; the realtime graph only reads an already-decoded ring buffer.
pub struct FileInputSource {
    ring: AudioRingBuffer,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    seek_sample: Arc<AtomicU64>,
    position_sample: Arc<AtomicU64>,
}

impl FileInputSource {
    pub fn open(path: impl AsRef<Path>, looped: bool, buffer_seconds: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        ensure_supported_extension(&path)?;
        std::fs::File::open(&path)?;

        let ring = AudioRingBuffer::new(buffer_seconds.max(1) * INTERNAL_SAMPLE_RATE as usize * 2);
        let writer = ring.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let paused_worker = paused.clone();
        let seek_sample = Arc::new(AtomicU64::new(NO_SEEK));
        let seek_worker = seek_sample.clone();
        let position_sample = Arc::new(AtomicU64::new(0));
        let position_worker = position_sample.clone();

        thread::Builder::new()
            .name("audio-file-decoder".into())
            .spawn(move || {
                let Ok(decoded) = decode_audio_file(&path) else {
                    return;
                };
                let samples = decoded.samples;
                let mut offset = 0usize;

                while !stop_worker.load(Ordering::Acquire) {
                    let request = seek_worker.swap(NO_SEEK, Ordering::AcqRel);
                    if request != NO_SEEK {
                        offset = (request as usize).min(samples.len());
                        writer.clear();
                        position_worker.store(offset as u64, Ordering::Release);
                    }
                    if paused_worker.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    if offset >= samples.len() {
                        if looped && !samples.is_empty() {
                            offset = 0;
                            position_worker.store(0, Ordering::Release);
                        } else {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    }

                    let end = (offset + 8192).min(samples.len());
                    let pushed = writer.push_slice(&samples[offset..end]);
                    if pushed == 0 {
                        thread::sleep(Duration::from_millis(3));
                    } else {
                        offset += pushed;
                        position_worker.store(offset as u64, Ordering::Release);
                    }
                }
            })
            .map_err(BackendError::Io)?;

        Ok(Self {
            ring,
            stop,
            paused,
            seek_sample,
            position_sample,
        })
    }

    /// Kept for source compatibility with pre-0.2 callers.
    pub fn open_wav(path: impl AsRef<Path>, looped: bool, buffer_seconds: usize) -> Result<Self> {
        Self::open(path, looped, buffer_seconds)
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn seek_seconds(&self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(BackendError::InvalidOperation(
                "seek position must be a finite non-negative value".into(),
            ));
        }
        let interleaved_sample = (seconds * INTERNAL_SAMPLE_RATE as f64 * 2.0).round() as u64;
        self.seek_sample
            .store(interleaved_sample, Ordering::Release);
        Ok(())
    }

    pub fn position_seconds(&self) -> f64 {
        self.position_sample.load(Ordering::Acquire) as f64 / (INTERNAL_SAMPLE_RATE as f64 * 2.0)
    }
}

impl AudioSource for FileInputSource {
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        let _ = self.ring.pop_into(&mut block.samples);
        Ok(())
    }
}

impl Drop for FileInputSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
