//! Narrowband FM (F3E) modulator for repeater work.
//!
//! Audio chain, at [`AUDIO_RATE`]: gain -> 6 dB/oct pre-emphasis -> clipper at
//! [`CLIP_LEVEL`] -> 4th-order 3 kHz low-pass (post-clipper, so clipping products are
//! filtered like a real radio's splatter filter) -> 4th-order 300 Hz high-pass (keeps voice
//! out of the sub-audible signaling band) -> safety clamp -> voice deviation, plus a
//! sub-audible CTCSS sine or DCS bitstream. The clipper sits below full scale because a low-passed clipped wave
//! overshoots by ~20% (the square wave's 4/pi fundamental); the clamp makes the peak
//! deviation a hard guarantee rather than a typical value.
//! The resulting instantaneous frequency is linearly interpolated up to the IQ rate and
//! integrated into phase.

use crate::AUDIO_RATE;
use crate::biquad::Biquad;
use crate::dcs::{DCS_BAUD, dcs_word};
use std::f64::consts::TAU;

/// Highest audio frequency passed; also the modulating frequency used for Carson's rule.
pub const AUDIO_CUTOFF_HZ: f32 = 3_000.0;

/// Clipper threshold, as a fraction of full voice deviation.
pub const CLIP_LEVEL: f32 = 0.8;

/// Lowest voice frequency passed; sub-audible signaling lives below it.
pub const VOICE_HIGHPASS_HZ: f32 = 300.0;

/// Sub-audible squelch signaling sent under the voice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SubTone {
    /// CTCSS / PL tone in Hz.
    Ctcss(f32),
    /// DCS code (0..=0o777); `inverted` sends the complement (the "I"/"N" suffix).
    Dcs { code: u16, inverted: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FmConfig {
    pub iq_rate: u32,
    /// Peak deviation of voice + sub-tone combined.
    pub peak_deviation_hz: f32,
    pub subtone: Option<SubTone>,
    /// Share of `peak_deviation_hz` given to the sub-tone.
    pub subtone_deviation_hz: f32,
    pub preemphasis: bool,
    /// Linear gain applied before the limiter.
    pub audio_gain: f32,
}

impl FmConfig {
    /// Standard repeater FM: +/-5 kHz peak, CTCSS/DCS at 600 Hz deviation.
    pub fn nbfm(iq_rate: u32, subtone: Option<SubTone>) -> Self {
        Self {
            iq_rate,
            peak_deviation_hz: 5_000.0,
            subtone,
            subtone_deviation_hz: 600.0,
            preemphasis: true,
            audio_gain: 1.0,
        }
    }

    /// Narrowband (12.5 kHz channel) FM: +/-2.5 kHz peak, sub-tone at 500 Hz.
    pub fn nbfm_narrow(iq_rate: u32, subtone: Option<SubTone>) -> Self {
        Self {
            peak_deviation_hz: 2_500.0,
            subtone_deviation_hz: 500.0,
            ..Self::nbfm(iq_rate, subtone)
        }
    }

    /// Carson's rule bandwidth, rounded up to the next kHz.
    pub fn occupied_bw_hz(&self) -> u32 {
        let carson = 2.0 * (self.peak_deviation_hz + AUDIO_CUTOFF_HZ);
        ((carson / 1000.0).ceil() * 1000.0) as u32
    }

    fn voice_deviation_hz(&self) -> f32 {
        match self.subtone {
            Some(_) => self.peak_deviation_hz - self.subtone_deviation_hz,
            None => self.peak_deviation_hz,
        }
    }
}

pub struct FmModulator {
    cfg: FmConfig,
    pre_alpha: f32,
    pre_norm: f32,
    pre_prev: f32,
    lpf: [Biquad; 2],
    hpf: [Biquad; 2],
    sub: SubToneGen,
    /// Instantaneous frequency (Hz) at the previous and current audio sample.
    f_prev: f32,
    f_cur: f32,
    /// Position between f_prev and f_cur, in audio samples.
    frac: f64,
    step: f64,
    phase: f64,
}

impl FmModulator {
    pub fn new(cfg: FmConfig) -> Self {
        let fs = AUDIO_RATE as f32;
        // 750 us time constant: the customary LMR/amateur NBFM pre-emphasis, ~212 Hz corner.
        let pre_alpha = (-1.0 / (750e-6 * fs)).exp();
        // Normalize so 1 kHz passes at unity gain.
        let w = std::f32::consts::TAU * 1_000.0 / fs;
        let pre_norm =
            1.0 / ((1.0 - pre_alpha * w.cos()).powi(2) + (pre_alpha * w.sin()).powi(2)).sqrt();
        // Butterworth 4th order = two biquads with these Qs.
        let lpf = [
            Biquad::lowpass(fs, AUDIO_CUTOFF_HZ, 0.541_196_1),
            Biquad::lowpass(fs, AUDIO_CUTOFF_HZ, 1.306_563),
        ];
        let hpf = [
            Biquad::highpass(fs, VOICE_HIGHPASS_HZ, 0.541_196_1),
            Biquad::highpass(fs, VOICE_HIGHPASS_HZ, 1.306_563),
        ];
        Self {
            cfg,
            pre_alpha,
            pre_norm,
            pre_prev: 0.0,
            lpf,
            hpf,
            sub: SubToneGen::new(cfg.subtone),
            f_prev: 0.0,
            f_cur: 0.0,
            frac: 0.0,
            step: f64::from(AUDIO_RATE) / f64::from(cfg.iq_rate),
            phase: 0.0,
        }
    }

    pub fn config(&self) -> &FmConfig {
        &self.cfg
    }

    fn audio_to_freq(&mut self, x: f32) -> f32 {
        let mut s = x * self.cfg.audio_gain;
        if self.cfg.preemphasis {
            let y = (s - self.pre_alpha * self.pre_prev) * self.pre_norm;
            self.pre_prev = s;
            s = y;
        }
        s = s.clamp(-CLIP_LEVEL, CLIP_LEVEL);
        for f in self.lpf.iter_mut().chain(&mut self.hpf) {
            s = f.process(s);
        }
        s = s.clamp(-1.0, 1.0);
        s * self.cfg.voice_deviation_hz() + self.sub.next() * self.cfg.subtone_deviation_hz
    }

    /// Modulate `audio` (mono, [`AUDIO_RATE`], nominally -1..1) and append cs8 IQ to `out`.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<i8>) {
        const SCALE: f64 = 120.0; // headroom below 127 for the DAC
        let k = TAU / f64::from(self.cfg.iq_rate);
        out.reserve((audio.len() as f64 / self.step) as usize * 2 + 4);
        for &x in audio {
            self.f_prev = self.f_cur;
            self.f_cur = self.audio_to_freq(x);
            let (a, b) = (f64::from(self.f_prev), f64::from(self.f_cur));
            while self.frac < 1.0 {
                let f = a + (b - a) * self.frac;
                self.phase = (self.phase + k * f) % TAU;
                let (s, c) = self.phase.sin_cos();
                out.push((c * SCALE).round() as i8);
                out.push((s * SCALE).round() as i8);
                self.frac += self.step;
            }
            self.frac -= 1.0;
        }
    }
}

/// Sub-audible signal generator, output nominally -1..1.
enum SubToneGen {
    None,
    Ctcss {
        phase: f64,
        step: f64,
    },
    Dcs {
        word: u32,
        bit: u32,
        clock: f64,
        lpf: Biquad,
    },
}

impl SubToneGen {
    fn new(t: Option<SubTone>) -> Self {
        let fs = f64::from(AUDIO_RATE);
        match t {
            None => Self::None,
            Some(SubTone::Ctcss(hz)) => Self::Ctcss {
                phase: 0.0,
                step: TAU * f64::from(hz) / fs,
            },
            Some(SubTone::Dcs { code, inverted }) => Self::Dcs {
                word: dcs_word(code, inverted),
                bit: 0,
                clock: 0.0,
                // Round the NRZ edges: keeps the bitstream's harmonics out of the voice band.
                lpf: Biquad::lowpass(AUDIO_RATE as f32, 300.0, std::f32::consts::FRAC_1_SQRT_2),
            },
        }
    }

    fn next(&mut self) -> f32 {
        match self {
            Self::None => 0.0,
            Self::Ctcss { phase, step } => {
                let v = phase.sin() as f32;
                *phase = (*phase + *step) % TAU;
                v
            }
            Self::Dcs {
                word,
                bit,
                clock,
                lpf,
            } => {
                let level = if *word >> *bit & 1 == 1 { 1.0 } else { -1.0 };
                *clock += DCS_BAUD / f64::from(AUDIO_RATE);
                if *clock >= 1.0 {
                    *clock -= 1.0;
                    *bit = (*bit + 1) % 23;
                }
                lpf.process(level).clamp(-1.0, 1.0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IQ_RATE: u32 = 2_400_000; // integer multiple of AUDIO_RATE keeps decimate() exact

    fn modulate(cfg: FmConfig, audio: &[f32]) -> Vec<i8> {
        let mut m = FmModulator::new(cfg);
        let mut out = Vec::new();
        m.process(audio, &mut out);
        out
    }

    /// Instantaneous frequency (Hz) of each cs8 sample via the phase-difference discriminator.
    fn demod(iq: &[i8]) -> Vec<f64> {
        let ph: Vec<f64> = iq
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[i, q]| f64::from(*q).atan2(f64::from(*i)))
            .collect();
        ph.windows(2)
            .map(|w| {
                let mut d = w[1] - w[0];
                if d > std::f64::consts::PI {
                    d -= TAU;
                }
                if d < -std::f64::consts::PI {
                    d += TAU;
                }
                d * f64::from(IQ_RATE) / TAU
            })
            .collect()
    }

    fn tone(hz: f32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * AUDIO_RATE as f32) as usize)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / AUDIO_RATE as f32).sin())
            .collect()
    }

    /// Average the demodulated frequency down to audio rate (removes 8-bit quantization noise).
    fn decimate(f: &[f64]) -> Vec<f64> {
        let n = (IQ_RATE / AUDIO_RATE) as usize;
        f.chunks_exact(n)
            .map(|c| c.iter().sum::<f64>() / n as f64)
            .collect()
    }

    /// Goertzel magnitude of `hz` in a signal sampled at AUDIO_RATE.
    fn goertzel(x: &[f64], hz: f64) -> f64 {
        let w = TAU * hz / f64::from(AUDIO_RATE);
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in x {
            let s = v + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() * 2.0 / x.len() as f64
    }

    #[test]
    fn output_length_matches_rate_ratio() {
        let out = modulate(FmConfig::nbfm(IQ_RATE, None), &vec![0.0; 48_000]);
        let n = out.len() / 2;
        assert!(
            (n as i64 - IQ_RATE as i64).abs() <= 2,
            "{n} IQ samples for 1 s"
        );
    }

    #[test]
    fn silence_is_a_steady_carrier() {
        let out = modulate(FmConfig::nbfm(IQ_RATE, None), &vec![0.0; 4_800]);
        let f = demod(&out);
        assert!(f.iter().all(|v| v.abs() < 1.0));
    }

    #[test]
    fn overdriven_voice_stays_within_peak_deviation() {
        let mut cfg = FmConfig::nbfm(IQ_RATE, Some(SubTone::Ctcss(146.2)));
        cfg.audio_gain = 10.0; // grossly overdriven: limiter must hold it
        let out = modulate(cfg, &tone(1_000.0, 0.5, 1.0));
        let f = decimate(&demod(&out));
        let peak = f[2_400..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        // Through 8-bit IQ the measurement itself is only good to ~+/-100 Hz; the exact
        // ceiling is checked pre-quantization in deviation_ceiling_is_exact.
        assert!(peak < 5_150.0, "peak deviation {peak:.0} Hz");
        assert!(
            peak > 4_500.0,
            "peak deviation {peak:.0} Hz - limiter too aggressive?"
        );
    }

    #[test]
    fn deviation_ceiling_is_exact() {
        for sub in [
            None,
            Some(SubTone::Ctcss(146.2)),
            Some(SubTone::Dcs {
                code: 0o244,
                inverted: false,
            }),
        ] {
            let mut cfg = FmConfig::nbfm(IQ_RATE, sub);
            cfg.audio_gain = 20.0;
            let mut m = FmModulator::new(cfg);
            let peak = tone(700.0, 1.0, 1.0)
                .iter()
                .chain(&tone(2_900.0, 1.0, 1.0))
                .map(|&x| m.audio_to_freq(x).abs())
                .fold(0.0f32, f32::max);
            assert!(peak <= 5_000.0 + 1e-3, "{sub:?}: {peak} Hz");
        }
    }

    #[test]
    fn ctcss_tone_is_present_at_its_deviation() {
        let out = modulate(
            FmConfig::nbfm(IQ_RATE, Some(SubTone::Ctcss(146.2))),
            &vec![0.0; 48_000],
        );
        let f = decimate(&demod(&out));
        let dev = goertzel(&f[4_800..], 146.2);
        assert!((dev - 600.0).abs() < 30.0, "CTCSS deviation {dev:.0} Hz");
        assert!(goertzel(&f[4_800..], 100.0) < 20.0);
    }

    #[test]
    fn carson_bandwidth() {
        assert_eq!(FmConfig::nbfm(IQ_RATE, None).occupied_bw_hz(), 16_000);
        assert_eq!(
            FmConfig::nbfm_narrow(IQ_RATE, None).occupied_bw_hz(),
            11_000
        );
    }

    /// Slice the demodulated, decimated audio at DCS bit centres and return 23 bits read
    /// LSB-first, starting one full word in (past filter start-up).
    fn recover_dcs_word(f: &[f64]) -> u32 {
        let delay = 24.0; // 300 Hz biquad group delay plus interpolation, in audio samples
        let spb = f64::from(AUDIO_RATE) / DCS_BAUD;
        (0..23).fold(0u32, |w, i| {
            let t = ((23 + i) as f64 + 0.5) * spb + delay;
            let level = f[t as usize];
            w | (u32::from(level > 0.0) << i)
        })
    }

    #[test]
    fn dcs_244_survives_modulation_with_voice_on_top() {
        let sub = SubTone::Dcs {
            code: 0o244,
            inverted: false,
        };
        let mut cfg = FmConfig::nbfm(IQ_RATE, Some(sub));
        cfg.audio_gain = 3.0; // loud voice must not corrupt the bits
        let out = modulate(cfg, &tone(1_000.0, 1.0, 1.0));
        // A receiver's DCS decoder low-passes the discriminator output well below voice.
        let mut lp = [
            Biquad::lowpass(48_000.0, 250.0, 0.541),
            Biquad::lowpass(48_000.0, 250.0, 1.307),
        ];
        let f: Vec<f64> = decimate(&demod(&out))
            .iter()
            .map(|&v| lp.iter_mut().fold(v as f32, |x, b| b.process(x)) as f64)
            .collect();
        let delay_lp = 70.0; // extra delay of the receive filter
        let spb = f64::from(AUDIO_RATE) / DCS_BAUD;
        let word = (0..23).fold(0u32, |w, i| {
            let t = ((23 + i) as f64 + 0.5) * spb + 24.0 + delay_lp;
            w | (u32::from(f[t as usize] > 0.0) << i)
        });
        assert_eq!(word, dcs_word(0o244, false), "got {word:06X}");
    }

    #[test]
    fn dcs_inverted_sends_the_complement() {
        let sub = SubTone::Dcs {
            code: 0o244,
            inverted: true,
        };
        let out = modulate(FmConfig::nbfm(IQ_RATE, Some(sub)), &vec![0.0; 24_000]);
        let f = decimate(&demod(&out));
        assert_eq!(recover_dcs_word(&f), dcs_word(0o244, true));
        assert_eq!(dcs_word(0o244, true), !dcs_word(0o244, false) & 0x7F_FFFF);
    }

    #[test]
    fn voice_highpass_keeps_low_audio_out_of_the_subtone_band() {
        let cfg = FmConfig::nbfm(IQ_RATE, None);
        let low = goertzel(
            &decimate(&demod(&modulate(cfg, &tone(134.0, 1.0, 0.5))))[9_600..],
            134.0,
        );
        let mid = goertzel(
            &decimate(&demod(&modulate(cfg, &tone(1_000.0, 1.0, 0.5))))[9_600..],
            1_000.0,
        );
        // >= 30 dB down relative to a mid-band tone of the same input level.
        assert!(
            low < mid / 31.6,
            "134 Hz at {low:.0} Hz dev vs 1 kHz at {mid:.0}"
        );
    }
}
