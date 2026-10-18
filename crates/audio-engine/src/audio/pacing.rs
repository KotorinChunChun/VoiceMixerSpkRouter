use std::time::{Duration, Instant};

/// 予定時刻を基準に進め、sleepの復帰遅れを毎ブロック積み上げない。
pub(crate) struct BlockPacer {
    deadline: Instant,
    period: Duration,
}

impl BlockPacer {
    pub fn new(start: Instant, period: Duration) -> Self {
        Self {
            deadline: start,
            period,
        }
    }

    pub fn finish_block(&mut self, now: Instant, period: Duration) -> (Duration, bool) {
        if self.period != period {
            self.deadline = now;
            self.period = period;
        }
        self.deadline += period;
        let lag = now.saturating_duration_since(self.deadline);
        let stalled = lag > (period * 4).max(Duration::from_millis(50));
        if stalled {
            // 長い停止分を無制限に連続処理しない。ライブ入力側が古いPCMを破棄する。
            self.deadline = now + period;
        }
        (self.deadline.saturating_duration_since(now), stalled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_late_wakeups_do_not_accumulate_audio_delay() {
        let start = Instant::now();
        let period = Duration::from_millis(10);
        let mut pacer = BlockPacer::new(start, period);
        let mut now = start;
        for _ in 0..30_000 {
            now += Duration::from_millis(1); // 処理時間
            let (wait, stalled) = pacer.finish_block(now, period);
            assert!(!stalled);
            now += wait + Duration::from_millis(2); // 復帰遅れ
        }
        assert_eq!(now.duration_since(start), Duration::from_millis(300_002));
    }

    #[test]
    fn long_stall_and_period_change_rebase_without_unbounded_catchup() {
        let start = Instant::now();
        let period = Duration::from_millis(10);
        let mut pacer = BlockPacer::new(start, period);
        let resumed = start + Duration::from_secs(2);
        assert_eq!(pacer.finish_block(resumed, period), (period, true));
        let changed = Duration::from_millis(20);
        assert_eq!(
            pacer.finish_block(resumed + period, changed),
            (changed, false)
        );
    }
}
