//! 同一時間窓のRMSを測り、大きい側だけを下げる試聴専用補正。
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComparisonReport {
    pub measured_seconds: f64,
    pub ready: bool,
    pub silent: bool,
    pub original_gain_db: f32,
    pub processed_gain_db: f32,
}
pub struct ComparisonMeter {
    revision: u64,
    samples: usize,
    original_energy: f64,
    processed_energy: f64,
    pub report: ComparisonReport,
}
impl Default for ComparisonMeter {
    fn default() -> Self {
        Self {
            revision: u64::MAX,
            samples: 0,
            original_energy: 0.0,
            processed_energy: 0.0,
            report: ComparisonReport::default(),
        }
    }
}
impl ComparisonMeter {
    pub fn observe(&mut self, revision: u64, original: &[f32], processed: &[f32]) {
        if self.revision != revision {
            *self = Self::default();
            self.revision = revision;
        }
        if self.report.ready {
            return;
        }
        // 最初の1秒はDSPのattack/releaseの助走、その後3秒の同一窓を使う。
        for (a, b) in original.iter().zip(processed) {
            if self.samples >= 96_000 {
                self.original_energy += f64::from(*a).powi(2);
                self.processed_energy += f64::from(*b).powi(2);
            }
            self.samples += 1;
            if self.samples == 384_000 {
                break;
            }
        }
        self.report.measured_seconds = self.samples.saturating_sub(96_000) as f64 / 96_000.0;
        if self.samples >= 384_000 {
            self.report.ready = true;
            self.report.silent =
                self.original_energy < 0.0000288 || self.processed_energy < 0.0000288;
            if !self.report.silent {
                let difference =
                    (10.0 * (self.processed_energy / self.original_energy).log10()) as f32;
                self.report.original_gain_db = difference.min(0.0).max(-120.0);
                self.report.processed_gain_db = (-difference).min(0.0).max(-120.0);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_only_attenuates_and_revision_invalidates() {
        let mut meter = ComparisonMeter::default();
        let raw = vec![0.25; 960];
        let processed = vec![0.5; 960];
        for _ in 0..400 {
            meter.observe(1, &raw, &processed);
        }
        assert!(meter.report.ready);
        assert!(!meter.report.silent);
        assert_eq!(meter.report.original_gain_db, 0.0);
        assert!((meter.report.processed_gain_db + 6.0206).abs() < 0.001);
        meter.observe(2, &raw, &processed);
        assert!(!meter.report.ready);
    }
    #[test]
    fn silence_never_amplifies() {
        let mut meter = ComparisonMeter::default();
        for _ in 0..400 {
            meter.observe(1, &[0.0; 960], &[0.5; 960]);
        }
        assert!(meter.report.silent);
        assert_eq!(meter.report.original_gain_db, 0.0);
    }
}
