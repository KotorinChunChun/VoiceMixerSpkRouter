//! 音声イベントのピークを50msごとに集約。音声データ自体は保持しない。
use super::config::Mode;
use audio_mixer_backend::audio::{engine::RuntimeEvent, graph::NodeId};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const INTERVAL: Duration = Duration::from_millis(50);
pub const WINDOW: Duration = Duration::from_secs(1);
pub const STALE: Duration = Duration::from_millis(500);
pub const BINS: usize = 20;
pub type Shared = Arc<Mutex<Option<Snapshot>>>;

#[derive(Debug, Clone)]
pub struct Sample {
    pub at: Instant,
    pub input: Option<f32>,
    pub outputs: [Option<f32>; 2],
}
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub generation: u64,
    pub mode: Mode,
    pub at: Instant,
    pub samples: Vec<Sample>,
    pub active: [bool; 3],
}
impl Snapshot {
    pub fn fresh(&self, now: Instant, generation: u64, mode: Mode) -> bool {
        self.generation == generation
            && self.mode == mode
            && now.saturating_duration_since(self.at) <= STALE
    }
    /// 時間軸を固定し、観測の遅れを履歴の引き伸ばしにしない。
    pub fn bins(&self, now: Instant) -> [[Option<f32>; BINS]; 3] {
        let mut bins = [[None; BINS]; 3];
        for sample in &self.samples {
            let age = now.saturating_duration_since(sample.at);
            if age >= WINDOW {
                continue;
            }
            let index = BINS - 1 - (age.as_millis() / INTERVAL.as_millis()) as usize;
            for (row, value) in [sample.input, sample.outputs[0], sample.outputs[1]]
                .into_iter()
                .enumerate()
            {
                if let Some(value) = value {
                    accumulate(&mut bins[row][index], value);
                }
            }
        }
        bins
    }
}
pub struct Collector {
    pub input: NodeId,
    outputs: [NodeId; 2],
    active: [bool; 3],
    pending: [Option<f32>; 3],
    samples: VecDeque<Sample>,
    last: Instant,
    pub observed: u64,
}
fn accumulate(target: &mut Option<f32>, peak: f32) {
    if peak.is_finite() && peak >= 0.0 {
        *target = Some(target.unwrap_or(0.0).max(peak));
    }
}
impl Collector {
    pub fn new(input: NodeId, outputs: [NodeId; 2]) -> Self {
        Self {
            input,
            outputs,
            active: [false; 3],
            pending: [None; 3],
            samples: VecDeque::new(),
            last: Instant::now(),
            observed: 0,
        }
    }
    pub fn reset(&mut self) {
        self.pending = [None; 3];
        self.samples.clear();
        self.last = Instant::now();
    }
    pub fn set_active(&mut self, input: bool, outputs: [bool; 2]) {
        self.reset();
        self.active = [input, outputs[0], outputs[1]];
    }
    pub fn observe(&mut self, event: &RuntimeEvent) {
        if let RuntimeEvent::NodeMeter { node_id, meter } = event {
            let row = if *node_id == self.input {
                0
            } else if let Some(index) = self.outputs.iter().position(|id| id == node_id) {
                index + 1
            } else {
                return;
            };
            if !self.active[row] {
                return;
            }
            accumulate(&mut self.pending[row], meter.peak);
            self.observed += 1;
        }
    }
    pub fn publish(&mut self, now: Instant, generation: u64, mode: Mode) -> Option<Snapshot> {
        if now.saturating_duration_since(self.last) < INTERVAL {
            return None;
        }
        self.last = now;
        self.samples
            .retain(|sample| now.saturating_duration_since(sample.at) < WINDOW);
        if self.samples.len() == BINS {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            at: now,
            input: self.pending[0].take(),
            outputs: [self.pending[1].take(), self.pending[2].take()],
        });
        Some(Snapshot {
            generation,
            mode,
            at: now,
            samples: self.samples.iter().cloned().collect(),
            active: self.active,
        })
    }
}
pub fn level_height(peak: f32) -> f32 {
    if !peak.is_finite() || peak <= 0.0 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_mixer_backend::audio::meter::MeterFrame;
    #[test]
    fn peaks_are_routed_bounded_and_expire_without_stretching_time() {
        let input = NodeId::new();
        let output = NodeId::new();
        let monitor = NodeId::new();
        let unrelated = NodeId::new();
        let mut c = Collector::new(input, [output, monitor]);
        c.set_active(true, [true, true]);
        let start = c.last;
        for (id, peak) in [
            (input, 0.2),
            (input, 0.7),
            (output, 0.3),
            (monitor, 1.0),
            (unrelated, 2.0),
            (output, f32::NAN),
        ] {
            c.observe(&RuntimeEvent::NodeMeter {
                node_id: id,
                meter: MeterFrame {
                    peak,
                    ..Default::default()
                },
            });
        }
        let s = c.publish(start + INTERVAL, 1, Mode::Browser).unwrap();
        assert_eq!(s.samples[0].input, Some(0.7));
        assert_eq!(s.samples[0].outputs, [Some(0.3), Some(1.0)]);
        assert_eq!(s.bins(s.at)[0][19], Some(0.7));
        assert_eq!(s.bins(s.at)[1][19], Some(0.3));
        assert_eq!(s.bins(s.at)[2][19], Some(1.0));
        assert!(s.fresh(s.at, 1, Mode::Browser));
        assert!(!s.fresh(s.at + STALE + INTERVAL, 1, Mode::Browser));
        assert!(!s.fresh(s.at, 2, Mode::Browser));
        assert!(!s.fresh(s.at, 1, Mode::Device));
        assert!(s.bins(s.at + WINDOW).iter().flatten().all(Option::is_none));
        for i in 2..=50 {
            c.publish(start + INTERVAL * i, 1, Mode::Browser);
        }
        assert_eq!(c.samples.len(), BINS);
        assert!(c
            .samples
            .iter()
            .all(|s| s.input.is_none() && s.outputs.iter().all(Option::is_none)));
        c.set_active(false, [false, true]);
        for id in [input, output, monitor] {
            c.observe(&RuntimeEvent::NodeMeter {
                node_id: id,
                meter: MeterFrame {
                    peak: 0.5,
                    ..Default::default()
                },
            });
        }
        let s = c.publish(c.last + INTERVAL, 1, Mode::Browser).unwrap();
        assert_eq!(s.samples[0].input, None);
        assert_eq!(s.samples[0].outputs, [None, Some(0.5)]);
        c.reset();
        assert!(c.samples.is_empty());
        assert_eq!(level_height(0.0), 0.0);
        assert_eq!(level_height(1.0), 1.0);
        assert!((level_height(0.03162278) - 0.5).abs() < 0.001);
    }
}
