//! Audio EQ Cookbook biquad (RBJ). Coefficients are computed off the audio
//! thread; the realtime path only filters with Direct Form II transposed.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BiquadType {
    LowShelf,
    Peaking,
    HighShelf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BiquadCoeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl BiquadCoeffs {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    pub fn mix(from: Self, to: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let u = 1.0 - t;
        Self {
            b0: from.b0 * u + to.b0 * t,
            b1: from.b1 * u + to.b1 * t,
            b2: from.b2 * u + to.b2 * t,
            a1: from.a1 * u + to.a1 * t,
            a2: from.a2 * u + to.a2 * t,
        }
    }

    /// Magnitude in dB at `freq_hz` for a cascade of sections.
    pub fn cascade_magnitude_db(sections: &[Self], freq_hz: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * (freq_hz / sample_rate.max(1.0));
        let (sin, cos) = w.sin_cos();
        let mut mag2 = 1.0_f64;
        for s in sections {
            let z1_re = cos as f64;
            let z1_im = -(sin as f64);
            let z2_re = (2.0 * w).cos() as f64;
            let z2_im = -((2.0 * w).sin() as f64);
            let num_re = f64::from(s.b0) + f64::from(s.b1) * z1_re + f64::from(s.b2) * z2_re;
            let num_im = f64::from(s.b1) * z1_im + f64::from(s.b2) * z2_im;
            let den_re = 1.0 + f64::from(s.a1) * z1_re + f64::from(s.a2) * z2_re;
            let den_im = f64::from(s.a1) * z1_im + f64::from(s.a2) * z2_im;
            let num = num_re * num_re + num_im * num_im;
            let den = (den_re * den_re + den_im * den_im).max(1.0e-30);
            mag2 *= num / den;
        }
        (10.0 * mag2.max(1.0e-30).log10()) as f32
    }
}

pub fn design(
    kind: BiquadType,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> BiquadCoeffs {
    let sr = sample_rate.max(1.0);
    let freq = freq_hz.clamp(20.0, 0.45 * sr);
    let q = q.clamp(0.1, 10.0);
    let a = 10.0_f32.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * freq / sr;
    let cos = w0.cos();
    let sin = w0.sin();
    let alpha = sin / (2.0 * q);
    let (b0, b1, b2, a0, a1, a2) = match kind {
        BiquadType::Peaking => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
        BiquadType::LowShelf => {
            let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha),
                (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha,
            )
        }
        BiquadType::HighShelf => {
            let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha),
                (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha,
            )
        }
    };
    let inv_a0 = 1.0 / a0.max(1.0e-12);
    BiquadCoeffs {
        b0: b0 * inv_a0,
        b1: b1 * inv_a0,
        b2: b2 * inv_a0,
        a1: a1 * inv_a0,
        a2: a2 * inv_a0,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    pub coeffs: BiquadCoeffs,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.coeffs.b0 * x + self.z1;
        self.z1 = self.coeffs.b1 * x - self.coeffs.a1 * y + self.z2;
        self.z2 = self.coeffs.b2 * x - self.coeffs.a2 * y;
        y
    }
}
