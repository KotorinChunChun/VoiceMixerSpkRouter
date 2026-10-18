//! File-backed shared-memory SPSC audio ring used by process plugins.
//!
//! The backing file is memory-mapped once during plugin preparation. Steady
//! state push/pop only touches shared memory and atomics; no file I/O occurs on
//! the audio thread. A separate mapping is used in each direction.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use memmap2::{MmapMut, MmapOptions};

use crate::error::{BackendError, Result};

pub const SHARED_AUDIO_RING_V2_MAGIC: [u8; 4] = *b"VMA2";
pub const SHARED_AUDIO_RING_V2_VERSION: u32 = 2;
pub const SHARED_AUDIO_RING_V2_MAX_PORTS: usize = 8;

const MAGIC: [u8; 8] = *b"ARPCM001";
const VERSION: u32 = 1;

#[repr(C, align(64))]
struct RingHeader {
    magic: [u8; 8],
    version: u32,
    sample_rate: u32,
    channels: u16,
    _reserved0: u16,
    capacity_samples: u64,
    read_index: AtomicU64,
    write_index: AtomicU64,
    _reserved: [u8; 16],
}

const HEADER_SIZE: usize = std::mem::size_of::<RingHeader>();

pub struct SharedAudioRing {
    path: PathBuf,
    file: File,
    map: MmapMut,
    owner: bool,
}

impl SharedAudioRing {
    pub fn create(
        path: impl AsRef<Path>,
        capacity_samples: usize,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self> {
        if capacity_samples < 2 {
            return Err(BackendError::Plugin(
                "shared audio ring capacity must be at least 2 samples".into(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let byte_len = HEADER_SIZE
            .checked_add(
                capacity_samples
                    .checked_mul(std::mem::size_of::<f32>())
                    .ok_or_else(|| BackendError::Plugin("shared ring size overflow".into()))?,
            )
            .ok_or_else(|| BackendError::Plugin("shared ring size overflow".into()))?;
        file.set_len(byte_len as u64)?;
        let mut map =
            unsafe { MmapOptions::new().len(byte_len).map_mut(&file) }.map_err(BackendError::Io)?;
        let header = RingHeader {
            magic: MAGIC,
            version: VERSION,
            sample_rate,
            channels,
            _reserved0: 0,
            capacity_samples: capacity_samples as u64,
            read_index: AtomicU64::new(0),
            write_index: AtomicU64::new(0),
            _reserved: [0; 16],
        };
        unsafe {
            std::ptr::write(map.as_mut_ptr().cast::<RingHeader>(), header);
        }
        map.flush().map_err(BackendError::Io)?;
        Ok(Self {
            path,
            file,
            map,
            owner: true,
        })
    }

    /// Opens a transport created by the host. This is primarily intended for
    /// Rust process-plugin implementations/SDK code.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let map = unsafe { MmapOptions::new().map_mut(&file) }.map_err(BackendError::Io)?;
        let this = Self {
            path,
            file,
            map,
            owner: false,
        };
        this.validate_header()?;
        Ok(this)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn sample_rate(&self) -> u32 {
        self.header().sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.header().channels
    }
    pub fn capacity_samples(&self) -> usize {
        self.header().capacity_samples as usize
    }

    pub fn available_read(&self) -> usize {
        let header = self.header();
        let read = header.read_index.load(Ordering::Acquire);
        let write = header.write_index.load(Ordering::Acquire);
        write.saturating_sub(read).min(header.capacity_samples) as usize
    }

    pub fn queued_frames(&self) -> usize {
        let channels = usize::from(self.channels().max(1));
        self.available_read() / channels
    }

    pub fn available_write(&self) -> usize {
        self.capacity_samples()
            .saturating_sub(self.available_read())
    }

    pub fn push(&mut self, input: &[f32]) -> usize {
        let capacity = self.capacity_samples();
        let header = self.header();
        let read = header.read_index.load(Ordering::Acquire);
        let write = header.write_index.load(Ordering::Relaxed);
        let available =
            capacity.saturating_sub(write.saturating_sub(read).min(capacity as u64) as usize);
        let count = available.min(input.len());
        if count == 0 {
            return 0;
        }
        let data = self.data_mut();
        for (i, sample) in input[..count].iter().enumerate() {
            data[(write as usize + i) % capacity] = *sample;
        }
        self.header()
            .write_index
            .store(write.wrapping_add(count as u64), Ordering::Release);
        count
    }

    pub fn pop(&mut self, output: &mut [f32]) -> usize {
        let capacity = self.capacity_samples();
        let header = self.header();
        let read = header.read_index.load(Ordering::Relaxed);
        let write = header.write_index.load(Ordering::Acquire);
        let count = (write.saturating_sub(read) as usize)
            .min(capacity)
            .min(output.len());
        if count == 0 {
            return 0;
        }
        let data = self.data();
        for (i, dst) in output[..count].iter_mut().enumerate() {
            *dst = data[(read as usize + i) % capacity];
        }
        self.header()
            .read_index
            .store(read.wrapping_add(count as u64), Ordering::Release);
        count
    }

    fn validate_header(&self) -> Result<()> {
        let h = self.header();
        if h.magic != MAGIC || h.version != VERSION {
            return Err(BackendError::Plugin(format!(
                "invalid shared audio transport header in {}",
                self.path.display()
            )));
        }
        let expected = HEADER_SIZE + h.capacity_samples as usize * std::mem::size_of::<f32>();
        if self.map.len() < expected {
            return Err(BackendError::Plugin(
                "shared audio transport is truncated".into(),
            ));
        }
        Ok(())
    }

    fn header(&self) -> &RingHeader {
        unsafe { &*self.map.as_ptr().cast::<RingHeader>() }
    }
    fn data(&self) -> &[f32] {
        let len = self.capacity_samples();
        unsafe { std::slice::from_raw_parts(self.map.as_ptr().add(HEADER_SIZE).cast::<f32>(), len) }
    }
    fn data_mut(&mut self) -> &mut [f32] {
        let len = self.capacity_samples();
        unsafe {
            std::slice::from_raw_parts_mut(
                self.map.as_mut_ptr().add(HEADER_SIZE).cast::<f32>(),
                len,
            )
        }
    }
}

impl Drop for SharedAudioRing {
    fn drop(&mut self) {
        let _ = self.map.flush();
        let _ = &self.file;
        if self.owner {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[repr(C, align(64))]
struct RingV2PortHeader {
    capacity_samples: u64,
    read_index: AtomicU64,
    write_index: AtomicU64,
    _reserved: [u8; 40],
}

impl RingV2PortHeader {
    fn new(capacity_samples: usize) -> Self {
        Self {
            capacity_samples: capacity_samples as u64,
            read_index: AtomicU64::new(0),
            write_index: AtomicU64::new(0),
            _reserved: [0; 40],
        }
    }
}

#[repr(C, align(64))]
struct RingV2Header {
    magic: [u8; 4],
    layout_version: u32,
    port_count: u32,
    sample_rate: u32,
    channels: u16,
    _reserved0: u16,
    capacity_samples: u64,
    ports: [RingV2PortHeader; SHARED_AUDIO_RING_V2_MAX_PORTS],
}

const V2_HEADER_SIZE: usize = std::mem::size_of::<RingV2Header>();

/// Version 2 shared-memory transport. Each port owns an independent SPSC
/// region in one mapping; the v1 `SharedAudioRing` layout is untouched.
pub struct SharedAudioRingV2 {
    path: PathBuf,
    file: File,
    map: MmapMut,
    owner: bool,
}

impl SharedAudioRingV2 {
    pub fn create(
        path: impl AsRef<Path>,
        port_count: usize,
        capacity_frames: usize,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self> {
        if !(1..=SHARED_AUDIO_RING_V2_MAX_PORTS).contains(&port_count) {
            return Err(BackendError::Plugin(format!(
                "shared audio ring v2 port count must be between 1 and {}",
                SHARED_AUDIO_RING_V2_MAX_PORTS
            )));
        }
        let channels = usize::from(channels.max(1));
        let capacity_samples = capacity_frames
            .checked_mul(channels)
            .ok_or_else(|| BackendError::Plugin("shared ring v2 size overflow".into()))?;
        if capacity_samples < 2 {
            return Err(BackendError::Plugin(
                "shared audio ring v2 capacity must be at least 2 samples".into(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let data_bytes = port_count
            .checked_mul(capacity_samples)
            .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
            .ok_or_else(|| BackendError::Plugin("shared ring v2 size overflow".into()))?;
        let byte_len = V2_HEADER_SIZE
            .checked_add(data_bytes)
            .ok_or_else(|| BackendError::Plugin("shared ring v2 size overflow".into()))?;
        file.set_len(byte_len as u64)?;
        let mut map =
            unsafe { MmapOptions::new().len(byte_len).map_mut(&file) }.map_err(BackendError::Io)?;
        let header = RingV2Header {
            magic: SHARED_AUDIO_RING_V2_MAGIC,
            layout_version: SHARED_AUDIO_RING_V2_VERSION,
            port_count: port_count as u32,
            sample_rate,
            channels: channels as u16,
            _reserved0: 0,
            capacity_samples: capacity_samples as u64,
            ports: std::array::from_fn(|_| RingV2PortHeader::new(capacity_samples)),
        };
        unsafe { std::ptr::write(map.as_mut_ptr().cast::<RingV2Header>(), header) };
        map.flush().map_err(BackendError::Io)?;
        Ok(Self {
            path,
            file,
            map,
            owner: true,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let map = unsafe { MmapOptions::new().map_mut(&file) }.map_err(BackendError::Io)?;
        let this = Self {
            path,
            file,
            map,
            owner: false,
        };
        this.validate_header()?;
        Ok(this)
    }

    pub fn port_count(&self) -> usize {
        self.header().port_count as usize
    }
    pub fn sample_rate(&self) -> u32 {
        self.header().sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.header().channels
    }
    pub fn capacity_frames(&self) -> usize {
        self.header().capacity_samples as usize / usize::from(self.channels().max(1))
    }
    pub fn available_read(&self, port: usize) -> Result<usize> {
        let header = self.port_header(port)?;
        let read = header.read_index.load(Ordering::Acquire);
        let write = header.write_index.load(Ordering::Acquire);
        Ok(write.saturating_sub(read).min(header.capacity_samples) as usize)
    }
    pub fn queued_frames(&self, port: usize) -> Result<usize> {
        Ok(self.available_read(port)? / usize::from(self.channels().max(1)))
    }
    pub fn push(&mut self, port: usize, input: &[f32]) -> Result<usize> {
        let capacity = self.header().capacity_samples as usize;
        let (read, write) = {
            let header = self.port_header(port)?;
            (
                header.read_index.load(Ordering::Acquire),
                header.write_index.load(Ordering::Relaxed),
            )
        };
        let available =
            capacity.saturating_sub(write.saturating_sub(read).min(capacity as u64) as usize);
        let count = available.min(input.len());
        if count == 0 {
            return Ok(0);
        }
        let data = self.port_data_mut(port)?;
        for (index, sample) in input[..count].iter().enumerate() {
            data[(write as usize + index) % capacity] = *sample;
        }
        self.port_header(port)?
            .write_index
            .store(write.wrapping_add(count as u64), Ordering::Release);
        Ok(count)
    }
    pub fn pop(&mut self, port: usize, output: &mut [f32]) -> Result<usize> {
        let capacity = self.header().capacity_samples as usize;
        let (read, write) = {
            let header = self.port_header(port)?;
            (
                header.read_index.load(Ordering::Relaxed),
                header.write_index.load(Ordering::Acquire),
            )
        };
        let count = (write.saturating_sub(read) as usize)
            .min(capacity)
            .min(output.len());
        if count == 0 {
            return Ok(0);
        }
        let data = self.port_data(port)?;
        for (index, dst) in output[..count].iter_mut().enumerate() {
            *dst = data[(read as usize + index) % capacity];
        }
        self.port_header(port)?
            .read_index
            .store(read.wrapping_add(count as u64), Ordering::Release);
        Ok(count)
    }

    fn validate_header(&self) -> Result<()> {
        if self.map.len() < V2_HEADER_SIZE {
            return Err(BackendError::Plugin(
                "shared audio ring v2 header is truncated".into(),
            ));
        }
        let h = self.header();
        if h.magic != SHARED_AUDIO_RING_V2_MAGIC {
            return Err(BackendError::Plugin(format!(
                "invalid shared audio ring v2 magic: expected VMA2, got {:?}",
                h.magic
            )));
        }
        if h.layout_version != SHARED_AUDIO_RING_V2_VERSION {
            return Err(BackendError::Plugin(format!(
                "invalid shared audio ring v2 version: expected {}, got {}",
                SHARED_AUDIO_RING_V2_VERSION, h.layout_version
            )));
        }
        if !(1..=SHARED_AUDIO_RING_V2_MAX_PORTS).contains(&(h.port_count as usize))
            || h.channels == 0
        {
            return Err(BackendError::Plugin(
                "invalid shared audio ring v2 metadata".into(),
            ));
        }
        let expected = V2_HEADER_SIZE
            .checked_add(
                h.port_count as usize * h.capacity_samples as usize * std::mem::size_of::<f32>(),
            )
            .ok_or_else(|| BackendError::Plugin("shared ring v2 size overflow".into()))?;
        if self.map.len() < expected {
            return Err(BackendError::Plugin(
                "shared audio ring v2 is truncated".into(),
            ));
        }
        Ok(())
    }
    fn header(&self) -> &RingV2Header {
        unsafe { &*self.map.as_ptr().cast::<RingV2Header>() }
    }
    fn port_header(&self, port: usize) -> Result<&RingV2PortHeader> {
        if port >= self.port_count() {
            return Err(BackendError::Plugin(format!(
                "shared audio ring v2 port {port} is out of range"
            )));
        }
        Ok(&self.header().ports[port])
    }
    fn port_data(&self, port: usize) -> Result<&[f32]> {
        self.port_header(port)?;
        let start = V2_HEADER_SIZE
            + port * self.header().capacity_samples as usize * std::mem::size_of::<f32>();
        let len = self.header().capacity_samples as usize;
        Ok(unsafe { std::slice::from_raw_parts(self.map.as_ptr().add(start).cast::<f32>(), len) })
    }
    fn port_data_mut(&mut self, port: usize) -> Result<&mut [f32]> {
        self.port_header(port)?;
        let start = V2_HEADER_SIZE
            + port * self.header().capacity_samples as usize * std::mem::size_of::<f32>();
        let len = self.header().capacity_samples as usize;
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.map.as_mut_ptr().add(start).cast::<f32>(), len)
        })
    }
}

impl Drop for SharedAudioRingV2 {
    fn drop(&mut self) {
        let _ = self.map.flush();
        let _ = &self.file;
        if self.owner {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod v2_tests {
    use super::SharedAudioRingV2;

    #[test]
    fn t07_u_04_v2_ring_round_trips_each_port_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring-v2.bin");
        let mut ring = SharedAudioRingV2::create(&path, 2, 8, 48_000, 2).unwrap();
        ring.push(0, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        ring.push(1, &[10.0, 20.0, 30.0, 40.0]).unwrap();
        let mut first = [0.0; 4];
        let mut second = [0.0; 4];
        assert_eq!(ring.pop(0, &mut first).unwrap(), 4);
        assert_eq!(ring.pop(1, &mut second).unwrap(), 4);
        assert_eq!(first, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(second, [10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn t07_u_05_v2_ring_rejects_magic_or_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring-v2-invalid.bin");
        let mut ring = SharedAudioRingV2::create(&path, 1, 8, 48_000, 2).unwrap();
        ring.map[0] = b'X';
        ring.map.flush().unwrap();
        let result = SharedAudioRingV2::open(&path);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("expected VMA2"));
        drop(ring);
    }
}
