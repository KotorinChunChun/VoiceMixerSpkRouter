#[cfg(windows)]
use crate::{
    audio::{buffer::AudioRingBuffer, clock::CaptureClock},
    error::{BackendError, Result},
};

#[cfg(windows)]
use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::Arc,
};

#[cfg(windows)]
use windows::{
    core::{PCWSTR, PSTR},
    Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        Media::{
            Audio::{
                waveInAddBuffer, waveInClose, waveInGetDevCapsW, waveInGetErrorTextW,
                waveInGetNumDevs, waveInOpen, waveInPrepareHeader, waveInReset, waveInStart,
                waveInStop, waveInUnprepareHeader, CALLBACK_EVENT, HWAVEIN, WAVEFORMATEX, WAVEHDR,
                WAVEINCAPSW, WAVE_FORMAT_PCM, WHDR_DONE,
            },
            MMSYSERR_NOERROR,
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
};

#[cfg(windows)]
const BUFFER_COUNT: usize = 4;
#[cfg(windows)]
const FRAMES_PER_BUFFER: usize = 480;

#[cfg(windows)]
fn mm_error(operation: &str, code: u32) -> BackendError {
    let mut message = [0u16; 256];
    let text = unsafe {
        if waveInGetErrorTextW(code, &mut message) == MMSYSERR_NOERROR {
            String::from_utf16_lossy(
                &message[..message
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(message.len())],
            )
        } else {
            format!("MMRESULT {code}")
        }
    };
    BackendError::Audio(format!("WinMM {operation}: {text}"))
}

#[cfg(windows)]
fn wave_in_device_id(endpoint_id: &str) -> Result<u32> {
    use wasapi::DeviceEnumerator;

    let enumerator =
        DeviceEnumerator::new().map_err(|error| BackendError::Audio(error.to_string()))?;
    let friendly_name = enumerator
        .get_device(endpoint_id)
        .and_then(|device| device.get_friendlyname())
        .map_err(|error| BackendError::Audio(format!("WinMM endpoint lookup: {error}")))?;
    let wanted = friendly_name.to_lowercase();
    let count = unsafe { waveInGetNumDevs() };
    let mut available = Vec::new();
    for device_id in 0..count {
        let mut caps = WAVEINCAPSW::default();
        let code = unsafe {
            waveInGetDevCapsW(
                device_id as usize,
                &mut caps,
                size_of::<WAVEINCAPSW>() as u32,
            )
        };
        if code != MMSYSERR_NOERROR {
            continue;
        }
        let product_name = unsafe { std::ptr::addr_of!(caps.szPname).read_unaligned() };
        let len = product_name
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(product_name.len());
        let name = String::from_utf16_lossy(&product_name[..len]);
        let normalized = name.to_lowercase();
        if wanted.starts_with(&normalized)
            || normalized.starts_with(&wanted)
            || wanted.contains(&normalized)
        {
            return Ok(device_id);
        }
        available.push(name);
    }
    Err(BackendError::Audio(format!(
        "WinMM device mapping failed for '{friendly_name}'. available: {}",
        available.join(", ")
    )))
}

/// One WinMM WaveIn session. The stream is 48 kHz PCM16 and is converted to
/// the internal 48 kHz stereo f32 representation before entering the graph.
#[cfg(windows)]
pub(crate) fn run_winmm_capture(
    endpoint_id: &str,
    ring: &AudioRingBuffer,
    stop: &Arc<AtomicBool>,
    dropped_samples: &Arc<AtomicU64>,
    clock: &CaptureClock,
    probe: &crate::audio::level_probe::LevelProbe,
    on_running: impl FnOnce(),
) -> Result<()> {
    let device_id = wave_in_device_id(endpoint_id)?;
    let sample_rate = 48_000u32;
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(|error| BackendError::Audio(format!("WinMM CreateEventW: {error}")))?;
    let mut handle = HWAVEIN::default();
    let mut channels = 0u16;
    let mut open_code = 1u32;
    // USB microphones commonly expose only a mono WaveIn format. Prefer
    // stereo, then duplicate a mono stream into the internal stereo graph.
    for candidate_channels in [2u16, 1u16] {
        let block_align = candidate_channels * 2;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: candidate_channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        handle = HWAVEIN::default();
        open_code = unsafe {
            waveInOpen(
                Some(&mut handle),
                device_id,
                &format,
                Some(event.0 as usize),
                None,
                CALLBACK_EVENT,
            )
        };
        if open_code == MMSYSERR_NOERROR {
            channels = candidate_channels;
            break;
        }
    }
    if open_code != MMSYSERR_NOERROR {
        unsafe { CloseHandle(event) }.ok();
        return Err(mm_error("waveInOpen", open_code));
    }

    let block_align = channels * 2;
    let bytes_per_buffer = FRAMES_PER_BUFFER * usize::from(block_align);
    let mut buffers = (0..BUFFER_COUNT)
        .map(|_| vec![0u8; bytes_per_buffer])
        .collect::<Vec<_>>();
    let mut headers = buffers
        .iter_mut()
        .map(|buffer| WAVEHDR {
            lpData: PSTR(buffer.as_mut_ptr()),
            dwBufferLength: buffer.len() as u32,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let header_size = size_of::<WAVEHDR>() as u32;
    let mut prepared = 0usize;
    let result = (|| -> Result<()> {
        for header in &mut headers {
            let code = unsafe { waveInPrepareHeader(handle, header, header_size) };
            if code != MMSYSERR_NOERROR {
                return Err(mm_error("waveInPrepareHeader", code));
            }
            prepared += 1;
            let code = unsafe { waveInAddBuffer(handle, header, header_size) };
            if code != MMSYSERR_NOERROR {
                return Err(mm_error("waveInAddBuffer", code));
            }
        }
        let code = unsafe { waveInStart(handle) };
        if code != MMSYSERR_NOERROR {
            return Err(mm_error("waveInStart", code));
        }
        on_running();

        while !stop.load(Ordering::Acquire) {
            if unsafe { WaitForSingleObject(event, 100) } != WAIT_OBJECT_0 {
                continue;
            }
            for header in &mut headers {
                let flags = unsafe { std::ptr::addr_of!(header.dwFlags).read_unaligned() };
                if flags & WHDR_DONE == 0 {
                    continue;
                }
                let recorded =
                    unsafe { std::ptr::addr_of!(header.dwBytesRecorded).read_unaligned() } as usize;
                let bytes = unsafe { std::slice::from_raw_parts(header.lpData.0, recorded) };
                let mut decoded = Vec::with_capacity(if channels == 1 {
                    recorded
                } else {
                    recorded / 2
                });
                for sample in bytes.chunks_exact(2) {
                    let value = i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0;
                    decoded.push(value);
                    if channels == 1 {
                        decoded.push(value);
                    }
                }
                let written = ring.push_slice(&decoded);
                clock.observe(
                    crate::audio::clock::now_100ns(),
                    crate::audio::clock::LatencyAccuracy::HostEstimated,
                );
                probe.observe(
                    &decoded,
                    crate::audio::clock::now_100ns().saturating_sub(
                        crate::audio::clock::frames_to_100ns(decoded.len() / 2, 48_000),
                    ),
                    48_000,
                    2,
                );
                if written < decoded.len() {
                    dropped_samples.fetch_add((decoded.len() - written) as u64, Ordering::Relaxed);
                }
                unsafe { std::ptr::addr_of_mut!(header.dwBytesRecorded).write_unaligned(0) };
                let code = unsafe { waveInAddBuffer(handle, header, header_size) };
                if code != MMSYSERR_NOERROR {
                    return Err(mm_error("waveInAddBuffer", code));
                }
            }
        }
        Ok(())
    })();

    unsafe {
        let _ = waveInStop(handle);
        let _ = waveInReset(handle);
        for header in headers.iter_mut().take(prepared) {
            let _ = waveInUnprepareHeader(handle, header, header_size);
        }
        let _ = waveInClose(handle);
        CloseHandle(event).ok();
    }
    result
}

#[cfg(not(windows))]
pub(crate) fn run_winmm_capture(
    _endpoint_id: &str,
    _ring: &crate::audio::buffer::AudioRingBuffer,
    _stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    _dropped_samples: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    _clock: &crate::audio::clock::CaptureClock,
    _probe: &crate::audio::level_probe::LevelProbe,
    _on_running: impl FnOnce(),
) -> crate::error::Result<()> {
    Err(crate::error::BackendError::PlatformUnsupported(
        "WinMM WaveIn requires Windows".into(),
    ))
}
