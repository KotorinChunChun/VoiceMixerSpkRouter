use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;

use crate::{
    audio::format::AudioBlock,
    error::Result,
    plugins::{
        builtins::{
            biquad::{design, Biquad, BiquadCoeffs, BiquadType},
            dsp_common::{
                api_version, audio_in_out_ports, bool_from, bool_param, json_bool, json_number,
                number_from, number_param, require_stereo, SmoothLin, GAIN_SMOOTH_MS, INTERNAL_SR,
            },
        },
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{PluginAudioRequirements, PluginDescriptor, PluginExecutionMode},
        process_protocol::PluginStats,
    },
};

pub const EQUALIZER_PLUGIN_ID: &str = "audio-router.builtin.equalizer";
pub const EQ_BANDS: usize = 5;
const DEFAULT_FREQS: [f32; EQ_BANDS] = [100.0, 250.0, 1_000.0, 4_000.0, 8_000.0];
const BAND_KINDS: [BiquadType; EQ_BANDS] = [
    BiquadType::LowShelf,
    BiquadType::Peaking,
    BiquadType::Peaking,
    BiquadType::Peaking,
    BiquadType::HighShelf,
];
const DEFAULT_Q: [f32; EQ_BANDS] = [0.7, 1.0, 1.0, 1.0, 0.7];

#[derive(Debug, Clone, Copy)]
pub struct EqBandParams {
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for EqBandParams {
    fn default() -> Self {
        Self {
            freq_hz: 1_000.0,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Equalizer {
    sample_rate: u32,
    enabled: bool,
    enabled_gain: SmoothLin,
    bands: [EqBandParams; EQ_BANDS],
    target_coeffs: [BiquadCoeffs; EQ_BANDS],
    current_coeffs: [BiquadCoeffs; EQ_BANDS],
    coeff_from: [BiquadCoeffs; EQ_BANDS],
    coeff_remaining: u32,
    coeff_total: u32,
    filters: Vec<[Biquad; EQ_BANDS]>,
    smoothing: bool,
}

impl Equalizer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate.max(1);
        let mut bands = [EqBandParams::default(); EQ_BANDS];
        for (i, band) in bands.iter_mut().enumerate() {
            band.freq_hz = DEFAULT_FREQS[i];
            band.q = DEFAULT_Q[i];
        }
        let coeffs = design_all(&bands, sr as f32);
        let ch = channels.max(1);
        Self {
            sample_rate: sr,
            enabled: true,
            enabled_gain: SmoothLin::new(1.0, sr),
            bands,
            target_coeffs: coeffs,
            current_coeffs: coeffs,
            coeff_from: coeffs,
            coeff_remaining: 0,
            coeff_total: 1,
            filters: vec![[Biquad::default(); EQ_BANDS]; ch],
            smoothing: true,
        }
    }

    pub fn set_smoothing(&mut self, enabled: bool) {
        self.smoothing = enabled;
        self.enabled_gain.set_enabled(enabled);
        if !enabled {
            self.current_coeffs = self.target_coeffs;
            self.coeff_remaining = 0;
            self.apply_coeffs_to_filters();
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.enabled_gain.set_sample_rate(self.sample_rate);
        self.recompute_target();
        self.current_coeffs = self.target_coeffs;
        self.coeff_remaining = 0;
        self.apply_coeffs_to_filters();
    }

    pub fn set_channels(&mut self, channels: usize) {
        let n = channels.max(1);
        if self.filters.len() != n {
            self.filters = vec![[Biquad::default(); EQ_BANDS]; n];
            self.apply_coeffs_to_filters();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.enabled_gain
            .set_target(if enabled { 1.0 } else { 0.0 });
    }

    pub fn set_band(&mut self, index: usize, params: EqBandParams) {
        if index >= EQ_BANDS {
            return;
        }
        let mut p = params;
        p.freq_hz = p.freq_hz.clamp(20.0, 20_000.0);
        p.gain_db = p.gain_db.clamp(-15.0, 15.0);
        p.q = p.q.clamp(0.1, 10.0);
        self.bands[index] = p;
        self.recompute_target();
        self.begin_coeff_glide();
    }

    pub fn band(&self, index: usize) -> EqBandParams {
        self.bands[index]
    }

    pub fn current_coeffs(&self) -> [BiquadCoeffs; EQ_BANDS] {
        self.current_coeffs
    }

    pub fn reset(&mut self) {
        for ch in &mut self.filters {
            for f in ch {
                f.reset();
            }
        }
        self.enabled_gain.snap(if self.enabled { 1.0 } else { 0.0 });
        self.current_coeffs = self.target_coeffs;
        self.coeff_remaining = 0;
        self.apply_coeffs_to_filters();
    }

    fn recompute_target(&mut self) {
        self.target_coeffs = design_all(&self.bands, self.sample_rate as f32);
    }

    fn begin_coeff_glide(&mut self) {
        if !self.smoothing {
            self.current_coeffs = self.target_coeffs;
            self.coeff_remaining = 0;
            self.apply_coeffs_to_filters();
            return;
        }
        self.coeff_from = self.current_coeffs;
        let n = ((self.sample_rate as f32) * GAIN_SMOOTH_MS / 1000.0).round() as u32;
        self.coeff_total = n.max(1);
        self.coeff_remaining = self.coeff_total;
    }

    fn tick_coeffs(&mut self) {
        if self.coeff_remaining == 0 {
            return;
        }
        let done = self.coeff_total - self.coeff_remaining + 1;
        let t = done as f32 / self.coeff_total as f32;
        for i in 0..EQ_BANDS {
            self.current_coeffs[i] =
                BiquadCoeffs::mix(self.coeff_from[i], self.target_coeffs[i], t);
        }
        self.coeff_remaining -= 1;
        self.apply_coeffs_to_filters();
    }

    fn apply_coeffs_to_filters(&mut self) {
        for ch in &mut self.filters {
            for (i, f) in ch.iter_mut().enumerate() {
                f.coeffs = self.current_coeffs[i];
            }
        }
    }

    pub fn process(&mut self, samples: &mut [f32], channels: usize) {
        self.set_channels(channels);
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        for frame in 0..frames {
            self.tick_coeffs();
            let wet = self.enabled_gain.next();
            let dry = 1.0 - wet;
            for ch in 0..channels {
                let index = frame * channels + ch;
                let x = samples[index];
                let mut y = x;
                if wet > 1.0e-6 {
                    for filter in &mut self.filters[ch] {
                        y = filter.process(y);
                    }
                }
                samples[index] = y * wet + x * dry;
            }
        }
    }
}

fn design_all(bands: &[EqBandParams; EQ_BANDS], sample_rate: f32) -> [BiquadCoeffs; EQ_BANDS] {
    let mut out = [BiquadCoeffs::identity(); EQ_BANDS];
    for i in 0..EQ_BANDS {
        out[i] = design(
            BAND_KINDS[i],
            bands[i].freq_hz,
            bands[i].gain_db,
            bands[i].q,
            sample_rate,
        );
    }
    out
}

pub struct EqualizerFactory;

impl EqualizerFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for EqualizerFactory {
    fn descriptor(&self) -> PluginDescriptor {
        let (input_ports, output_ports) = audio_in_out_ports();
        let mut parameters = vec![bool_param(
            "enabled",
            "有効",
            true,
            "基本",
            "EQ全体のバイパスです。",
        )];
        let labels = [
            "Band 1 (Low Shelf)",
            "Band 2 (Peak)",
            "Band 3 (Peak)",
            "Band 4 (Peak)",
            "Band 5 (High Shelf)",
        ];
        for i in 0..EQ_BANDS {
            let n = i + 1;
            let group = labels[i];
            parameters.push(number_param(
                &format!("band{n}_freq_hz"),
                "周波数",
                DEFAULT_FREQS[i] as f64,
                20.0,
                20_000.0,
                group,
                "Hz",
                1.0,
                "バンド中心周波数です。",
            ));
            parameters.push(number_param(
                &format!("band{n}_gain_db"),
                "ゲイン",
                0.0,
                -15.0,
                15.0,
                group,
                "dB",
                0.1,
                "バンドのブースト／カットです。",
            ));
            parameters.push(number_param(
                &format!("band{n}_q"),
                "Q",
                DEFAULT_Q[i] as f64,
                0.1,
                10.0,
                group,
                "Q",
                0.01,
                "帯域の鋭さです。",
            ));
        }
        PluginDescriptor {
            plugin_id: EQUALIZER_PLUGIN_ID.into(),
            name: "EQ".into(),
            version: "1.0.0".into(),
            api_version: api_version(),
            category: "eq".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports,
            output_ports,
            parameters,
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            default_enabled: false,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        Ok(Box::new(EqualizerInstance::from_parameters(parameters)?))
    }
}

struct EqualizerInstance {
    dsp: Equalizer,
}

impl EqualizerInstance {
    fn from_parameters(parameters: &BTreeMap<String, Value>) -> Result<Self> {
        let mut dsp = Equalizer::new(INTERNAL_SR, 2);
        apply_eq_parameters(&mut dsp, parameters)?;
        Ok(Self { dsp })
    }
}

fn apply_eq_parameters(dsp: &mut Equalizer, parameters: &BTreeMap<String, Value>) -> Result<()> {
    dsp.set_enabled(bool_from(parameters, "enabled", true)?);
    for i in 0..EQ_BANDS {
        let n = i + 1;
        let mut band = dsp.band(i);
        band.freq_hz = number_from(parameters, &format!("band{n}_freq_hz"), DEFAULT_FREQS[i])?;
        band.gain_db = number_from(parameters, &format!("band{n}_gain_db"), 0.0)?;
        band.q = number_from(parameters, &format!("band{n}_q"), DEFAULT_Q[i])?;
        dsp.set_band(i, band);
    }
    dsp.reset();
    Ok(())
}

impl TransformInstance for EqualizerInstance {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()> {
        require_stereo(context.channels)?;
        self.dsp.set_sample_rate(context.sample_rate);
        self.dsp.set_channels(context.channels as usize);
        self.dsp.reset();
        Ok(())
    }

    fn process(&mut self, block: &mut AudioBlock) -> Result<()> {
        let channels = block.format.channels as usize;
        self.dsp.process(&mut block.samples, channels);
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()> {
        if id == "enabled" {
            self.dsp.set_enabled(json_bool(&value)?);
            return Ok(());
        }
        for i in 0..EQ_BANDS {
            let n = i + 1;
            let mut band = self.dsp.band(i);
            if id == format!("band{n}_freq_hz") {
                band.freq_hz = json_number(&value)?;
                self.dsp.set_band(i, band);
                return Ok(());
            }
            if id == format!("band{n}_gain_db") {
                band.gain_db = json_number(&value)?;
                self.dsp.set_band(i, band);
                return Ok(());
            }
            if id == format!("band{n}_q") {
                band.q = json_number(&value)?;
                self.dsp.set_band(i, band);
                return Ok(());
            }
        }
        Err(crate::error::BackendError::Plugin(format!(
            "unknown equalizer parameter '{id}'"
        )))
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        None
    }
}

pub fn peak_band_1k_plus6() -> Equalizer {
    let mut eq = Equalizer::new(INTERNAL_SR, 1);
    eq.set_smoothing(false);
    for i in 0..EQ_BANDS {
        let mut band = eq.band(i);
        band.gain_db = 0.0;
        if i == 2 {
            band.freq_hz = 1_000.0;
            band.gain_db = 6.0;
            band.q = 1.0;
        }
        eq.set_band(i, band);
    }
    eq.reset();
    eq
}
