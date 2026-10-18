use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::audio::graph::NodeId;

/// 100-nanosecond units per second (Windows FILETIME / WASAPI QPC position).
pub const UNITS_100NS_PER_SEC: u64 = 10_000_000;

#[derive(Debug, Clone)]
pub struct DriftController {
    target_fill: f32,
    kp: f32,
    max_adjustment: f32,
}

impl DriftController {
    pub fn new(target_fill: f32) -> Self {
        Self {
            target_fill,
            kp: 0.002,
            max_adjustment: 0.005,
        }
    }

    /// Returns a resampling ratio correction around 1.0. A fuller-than-target
    /// buffer slightly speeds consumption; a lower buffer slightly slows it.
    pub fn correction_ratio(&self, current_fill: f32) -> f32 {
        let error = current_fill - self.target_fill;
        (1.0 + error * self.kp).clamp(1.0 - self.max_adjustment, 1.0 + self.max_adjustment)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LatencyAccuracy {
    DeviceReported,
    HostEstimated,
}

impl LatencyAccuracy {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::DeviceReported => 0,
            Self::HostEstimated => 1,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::DeviceReported,
            _ => Self::HostEstimated,
        }
    }

    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::DeviceReported, Self::DeviceReported) => Self::DeviceReported,
            _ => Self::HostEstimated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LatencyStage {
    Capture,
    Graph,
    Plugin,
    Render,
    EndToEnd,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct LatencySnapshot {
    pub last_ms: f64,
    pub ema_ms: f64,
    pub max_ms: f64,
}

impl LatencySnapshot {
    pub fn from_us(last_us: u32, ema_us: u32, max_us: u32) -> Self {
        Self {
            last_ms: last_us as f64 / 1000.0,
            ema_ms: ema_us as f64 / 1000.0,
            max_ms: max_us as f64 / 1000.0,
        }
    }

    pub fn is_finite(&self) -> bool {
        self.last_ms.is_finite() && self.ema_ms.is_finite() && self.max_ms.is_finite()
    }
}

/// Lock-free delay statistics. Same shape as `AtomicMeter`: realtime writers,
/// UI-thread readers, no mutex.
#[derive(Debug, Default)]
pub struct LatencyStats {
    last_us: AtomicU32,
    ema_us: AtomicU32,
    max_us: AtomicU32,
    initialized: AtomicBool,
}

impl LatencyStats {
    pub fn record_us(&self, us: u32) {
        self.last_us.store(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
        if self
            .initialized
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.ema_us.store(us, Ordering::Relaxed);
            return;
        }
        let ema = self.ema_us.load(Ordering::Relaxed);
        let next = ((u64::from(ema) * 9) + u64::from(us)) / 10;
        self.ema_us
            .store(next.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
    }

    pub fn last_us(&self) -> u32 {
        self.last_us.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot::from_us(
            self.last_us.load(Ordering::Relaxed),
            self.ema_us.load(Ordering::Relaxed),
            self.max_us.load(Ordering::Relaxed),
        )
    }

    pub fn reset(&self) {
        self.last_us.store(0, Ordering::Relaxed);
        self.ema_us.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
        self.initialized.store(false, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub struct LatencyBank {
    pub capture: LatencyStats,
    pub graph: LatencyStats,
    pub plugin_queue: LatencyStats,
    pub plugin_algo: LatencyStats,
    pub render: LatencyStats,
    pub e2e: LatencyStats,
    accuracy: AtomicU8,
}

impl LatencyBank {
    pub fn set_accuracy(&self, accuracy: LatencyAccuracy) {
        self.accuracy.store(accuracy.as_u8(), Ordering::Relaxed);
    }

    pub fn accuracy(&self) -> LatencyAccuracy {
        LatencyAccuracy::from_u8(self.accuracy.load(Ordering::Relaxed))
    }

    pub fn plugin_combined_us(&self) -> u32 {
        self.plugin_queue
            .last_us()
            .saturating_add(self.plugin_algo.last_us())
    }
}

#[derive(Debug, Default)]
pub struct CaptureClock {
    time_100ns: AtomicU64,
    accuracy: AtomicU8,
}

impl CaptureClock {
    pub fn observe(&self, time_100ns: u64, accuracy: LatencyAccuracy) {
        self.time_100ns.store(time_100ns, Ordering::Relaxed);
        self.accuracy.store(accuracy.as_u8() + 1, Ordering::Relaxed);
    }

    pub fn stamp(&self) -> Option<(u64, LatencyAccuracy)> {
        let tagged = self.accuracy.load(Ordering::Relaxed);
        if tagged == 0 {
            return None;
        }
        Some((
            self.time_100ns.load(Ordering::Relaxed),
            LatencyAccuracy::from_u8(tagged - 1),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PathLatencyStage {
    pub node_id: NodeId,
    pub stage: LatencyStage,
    pub snapshot: LatencySnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PathLatencyReport {
    pub input_node: NodeId,
    pub output_node: NodeId,
    pub e2e: LatencySnapshot,
    pub stages: Vec<PathLatencyStage>,
    pub accuracy: LatencyAccuracy,
}

pub fn frames_to_ms(frames: usize, sample_rate: u32) -> f64 {
    if sample_rate == 0 {
        return 0.0;
    }
    (frames as f64) * 1000.0 / f64::from(sample_rate)
}

pub fn frames_to_us(frames: usize, sample_rate: u32) -> u32 {
    frames_to_ms(frames, sample_rate)
        .mul_add(1000.0, 0.5)
        .clamp(0.0, u32::MAX as f64) as u32
}

pub fn frames_to_100ns(frames: usize, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (frames as u128 * u128::from(UNITS_100NS_PER_SEC) / u128::from(sample_rate)) as u64
}

pub fn qpc_ticks_to_100ns(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    (u128::from(ticks) * u128::from(UNITS_100NS_PER_SEC) / u128::from(frequency)) as u64
}

pub fn now_100ns() -> u64 {
    #[cfg(windows)]
    {
        let (ticks, frequency) = qpc_counter();
        qpc_ticks_to_100ns(ticks, frequency)
    }
    #[cfg(not(windows))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        (nanos / 100).min(u128::from(u64::MAX)) as u64
    }
}

pub fn qpc_counter() -> (u64, u64) {
    #[cfg(windows)]
    {
        qpc_raw()
    }
    #[cfg(not(windows))]
    {
        (0, 1)
    }
}

#[cfg(windows)]
fn qpc_raw() -> (u64, u64) {
    let mut counter = 0i64;
    let mut frequency = 0i64;
    let ok_counter = unsafe { QueryPerformanceCounter(&mut counter) };
    let ok_freq = unsafe { QueryPerformanceFrequency(&mut frequency) };
    if ok_counter == 0 || ok_freq == 0 || frequency <= 0 {
        return (0, 1);
    }
    (counter as u64, frequency as u64)
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(lp_performance_count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(lp_frequency: *mut i64) -> i32;
}

/// Map a WASAPI packet timestamp to 100 ns units. `device_time_100ns` is the
/// API's already-converted QPC position. A zero value or `timestamp_error`
/// falls back to a host QPC converted with the same 100 ns scale.
pub fn resolve_capture_timestamp(
    device_time_100ns: u64,
    timestamp_error: bool,
    host_qpc_ticks: u64,
    host_qpc_frequency: u64,
) -> (u64, LatencyAccuracy) {
    if device_time_100ns != 0 && !timestamp_error {
        (device_time_100ns, LatencyAccuracy::DeviceReported)
    } else {
        (
            qpc_ticks_to_100ns(host_qpc_ticks, host_qpc_frequency),
            LatencyAccuracy::HostEstimated,
        )
    }
}

pub fn oldest_capture_time(times: &[Option<u64>]) -> Option<u64> {
    times.iter().copied().flatten().min()
}

pub fn duration_100ns_to_us(delta_100ns: u64) -> u32 {
    (delta_100ns / 10).min(u64::from(u32::MAX)) as u32
}

pub type LatencyMap = std::collections::HashMap<NodeId, Arc<LatencyBank>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn t01_u_01_record_us_ema_matches_definition() {
        let stats = LatencyStats::default();
        stats.record_us(10_000);
        let first = stats.snapshot();
        assert!((first.last_ms - 10.0).abs() < 0.001);
        assert!((first.ema_ms - 10.0).abs() < 0.001);
        assert!((first.max_ms - 10.0).abs() < 0.001);

        stats.record_us(20_000);
        let second = stats.snapshot();
        assert!((second.last_ms - 20.0).abs() < 0.001);
        assert!((second.ema_ms - 11.0).abs() < 0.001);
        assert!((second.max_ms - 20.0).abs() < 0.001);
    }

    #[test]
    fn t01_u_02_concurrent_record_and_snapshot() {
        let stats = Arc::new(LatencyStats::default());
        let mut joins = Vec::new();
        for thread_index in 0..4 {
            let stats = stats.clone();
            joins.push(thread::spawn(move || {
                for i in 0..100_000u32 {
                    stats.record_us(1_000 + (thread_index * 10 + i % 50));
                    if i % 17 == 0 {
                        let snap = stats.snapshot();
                        assert!(snap.is_finite());
                        assert!(snap.last_ms >= 0.0);
                        assert!(snap.ema_ms >= 0.0);
                        assert!(snap.max_ms >= 0.0);
                    }
                }
            }));
        }
        for join in joins {
            join.join().expect("latency stats thread");
        }
        let snap = stats.snapshot();
        assert!(snap.is_finite());
        assert!(snap.last_ms >= 0.0);
        assert!(snap.max_ms >= snap.last_ms || snap.max_ms >= 1.0);
    }

    #[test]
    fn t01_u_03_frames_to_ms_480_at_48k() {
        let ms = frames_to_ms(480, 48_000);
        assert!((ms - 10.0).abs() < 0.001);
    }

    #[test]
    fn t01_u_04_oldest_capture_time_ignores_none() {
        assert_eq!(oldest_capture_time(&[Some(100), Some(50), None]), Some(50));
        assert_eq!(oldest_capture_time(&[None, None]), None);
        assert_eq!(oldest_capture_time(&[]), None);
    }

    #[test]
    fn t01_u_05_qpc_zero_and_timestamp_error_fall_back() {
        let frequency = 10_000_000u64;
        let (device, accuracy) = resolve_capture_timestamp(12_345, false, 0, frequency);
        assert_eq!(device, 12_345);
        assert_eq!(accuracy, LatencyAccuracy::DeviceReported);

        let mut previous = 0u64;
        for ticks in [frequency, frequency * 2, frequency * 3] {
            let (host, accuracy) = resolve_capture_timestamp(0, false, ticks, frequency);
            assert_eq!(accuracy, LatencyAccuracy::HostEstimated);
            assert_eq!(host, ticks);
            assert!(host >= previous);
            previous = host;
        }

        let (errored, accuracy) = resolve_capture_timestamp(99, true, frequency, frequency);
        assert_eq!(accuracy, LatencyAccuracy::HostEstimated);
        assert_eq!(errored, UNITS_100NS_PER_SEC);
        assert_eq!(
            qpc_ticks_to_100ns(frequency, frequency),
            UNITS_100NS_PER_SEC
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]
        #[test]
        fn t01_p_01_frames_to_ms_non_negative_monotonic(
            frames in 0usize..1_000_000,
            extra in 0usize..10_000,
            sample_rate in 1u32..=192_000,
        ) {
            let a = frames_to_ms(frames, sample_rate);
            let b = frames_to_ms(frames.saturating_add(extra), sample_rate);
            assert!(a.is_finite() && a >= 0.0);
            assert!(b.is_finite() && b >= a);
            let hundred_ns = frames_to_100ns(frames, sample_rate);
            let later = frames_to_100ns(frames.saturating_add(extra), sample_rate);
            assert!(later >= hundred_ns);
        }
    }
}
