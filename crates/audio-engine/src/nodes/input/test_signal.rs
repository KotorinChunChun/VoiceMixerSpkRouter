use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering},
    Arc,
};

use parking_lot::Mutex;

use crate::{
    audio::graph::{TestSignalChannelMode, TestSignalConfig, TestSignalType},
    audio::{
        engine::AudioSource,
        format::{db_to_linear, AudioBlock, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
    },
    error::Result,
};

struct AtomicTestSignalConfig {
    signal_type: AtomicU8,
    frequency_bits: AtomicU32,
    level_bits: AtomicU32,
    channel_mode: AtomicU8,
    enabled: AtomicBool,
}

impl AtomicTestSignalConfig {
    fn new(config: TestSignalConfig) -> Self {
        Self {
            signal_type: AtomicU8::new(signal_type_to_u8(config.signal_type)),
            frequency_bits: AtomicU32::new(config.frequency_hz.to_bits()),
            level_bits: AtomicU32::new(config.level_db.to_bits()),
            channel_mode: AtomicU8::new(channel_mode_to_u8(config.channel_mode)),
            enabled: AtomicBool::new(config.enabled),
        }
    }

    fn store(&self, config: TestSignalConfig) {
        self.frequency_bits
            .store(config.frequency_hz.to_bits(), Ordering::Release);
        self.level_bits
            .store(config.level_db.to_bits(), Ordering::Release);
        self.channel_mode
            .store(channel_mode_to_u8(config.channel_mode), Ordering::Release);
        self.signal_type
            .store(signal_type_to_u8(config.signal_type), Ordering::Release);
        self.enabled.store(config.enabled, Ordering::Release);
    }

    fn load(&self) -> TestSignalConfig {
        TestSignalConfig {
            signal_type: signal_type_from_u8(self.signal_type.load(Ordering::Acquire)),
            frequency_hz: f32::from_bits(self.frequency_bits.load(Ordering::Acquire)),
            level_db: f32::from_bits(self.level_bits.load(Ordering::Acquire)),
            channel_mode: channel_mode_from_u8(self.channel_mode.load(Ordering::Acquire)),
            enabled: self.enabled.load(Ordering::Acquire),
        }
    }
}

struct GeneratorState {
    phase: f32,
    current_level: f32,
    rng: u64,
    pink_b0: f32,
    pink_b1: f32,
    pink_b2: f32,
    generated_frames: u64,
}

/// Stateful, allocation-free test source. Configuration writes never lock the
/// audio thread; oscillator, PRNG and pink-filter state remain continuous.
pub struct TestSignalSource {
    config: AtomicTestSignalConfig,
    state: Mutex<GeneratorState>,
}

impl TestSignalSource {
    pub fn new(config: TestSignalConfig) -> Arc<Self> {
        Arc::new(Self {
            config: AtomicTestSignalConfig::new(config),
            state: Mutex::new(GeneratorState {
                phase: 0.0,
                current_level: db_to_linear(config.level_db),
                rng: 0x9e37_79b9_7f4a_7c15,
                pink_b0: 0.0,
                pink_b1: 0.0,
                pink_b2: 0.0,
                generated_frames: 0,
            }),
        })
    }

    pub fn update_config(&self, config: TestSignalConfig) {
        self.config.store(config);
    }
}

impl AudioSource for TestSignalSource {
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        let config = self.config.load();
        if !config.enabled {
            block.clear();
            return Ok(());
        }
        let channels = block.format.channels as usize;
        if channels == 0 {
            return Ok(());
        }
        let mut state = self.state.lock();
        let phase_step = std::f32::consts::TAU * config.frequency_hz / INTERNAL_SAMPLE_RATE as f32;
        let target_level = db_to_linear(config.level_db);
        let smoothing = 1.0 / (INTERNAL_SAMPLE_RATE as f32 * 0.005).max(1.0);
        for frame in block.samples.chunks_exact_mut(channels) {
            state.current_level += (target_level - state.current_level) * smoothing;
            let raw = match config.signal_type {
                TestSignalType::Sine | TestSignalType::LeftRightTest => state.phase.sin(),
                TestSignalType::WhiteNoise => next_white(&mut state.rng),
                TestSignalType::PinkNoise => {
                    let white = next_white(&mut state.rng);
                    state.pink_b0 = 0.99765 * state.pink_b0 + white * 0.099_046;
                    state.pink_b1 = 0.96300 * state.pink_b1 + white * 0.296_516_4;
                    state.pink_b2 = 0.57000 * state.pink_b2 + white * 1.052_691_3;
                    (state.pink_b0 + state.pink_b1 + state.pink_b2 + white * 0.1848) * 0.11
                }
            } * state.current_level;
            state.phase = (state.phase + phase_step) % std::f32::consts::TAU;

            let alternating = config.signal_type == TestSignalType::LeftRightTest
                || config.channel_mode == TestSignalChannelMode::LeftRightAlternating;
            let left_active = (state.generated_frames / (INTERNAL_SAMPLE_RATE as u64 / 2)) % 2 == 0;
            let (left, right) = if alternating {
                if left_active {
                    (raw, 0.0)
                } else {
                    (0.0, raw)
                }
            } else {
                match config.channel_mode {
                    TestSignalChannelMode::Mono | TestSignalChannelMode::Stereo => (raw, raw),
                    TestSignalChannelMode::LeftOnly => (raw, 0.0),
                    TestSignalChannelMode::RightOnly => (0.0, raw),
                    TestSignalChannelMode::LeftRightAlternating => unreachable!(),
                }
            };
            frame[0] = left;
            if channels >= INTERNAL_CHANNELS as usize {
                frame[1] = right;
            }
            for sample in frame.iter_mut().skip(2) {
                *sample = 0.0;
            }
            state.generated_frames = state.generated_frames.saturating_add(1);
        }
        Ok(())
    }
}

impl AudioSource for Arc<TestSignalSource> {
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        self.as_ref().read_into(block)
    }
}

fn next_white(state: &mut u64) -> f32 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    ((value >> 40) as f32 / 8_388_607.5) - 1.0
}

fn signal_type_to_u8(value: TestSignalType) -> u8 {
    match value {
        TestSignalType::Sine => 0,
        TestSignalType::WhiteNoise => 1,
        TestSignalType::PinkNoise => 2,
        TestSignalType::LeftRightTest => 3,
    }
}

fn signal_type_from_u8(value: u8) -> TestSignalType {
    match value {
        1 => TestSignalType::WhiteNoise,
        2 => TestSignalType::PinkNoise,
        3 => TestSignalType::LeftRightTest,
        _ => TestSignalType::Sine,
    }
}

fn channel_mode_to_u8(value: TestSignalChannelMode) -> u8 {
    match value {
        TestSignalChannelMode::Mono => 0,
        TestSignalChannelMode::Stereo => 1,
        TestSignalChannelMode::LeftOnly => 2,
        TestSignalChannelMode::RightOnly => 3,
        TestSignalChannelMode::LeftRightAlternating => 4,
    }
}

fn channel_mode_from_u8(value: u8) -> TestSignalChannelMode {
    match value {
        0 => TestSignalChannelMode::Mono,
        2 => TestSignalChannelMode::LeftOnly,
        3 => TestSignalChannelMode::RightOnly,
        4 => TestSignalChannelMode::LeftRightAlternating,
        _ => TestSignalChannelMode::Stereo,
    }
}
