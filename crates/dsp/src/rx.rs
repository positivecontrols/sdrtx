//! Narrowband FM receiver: HackRF cs8 IQ in, 48 kHz speaker audio out.
//!
//! The HackRF is tuned [`RX_LO_OFFSET_HZ`] below the channel so its DC spike / LO leakage
//! stays out of the passband; the channel is mixed back to 0 Hz here. Then:
//! FIR /10 (2.4 Msps -> 240 ksps) -> FIR channel filter /5 (-> 48 ksps, +/-8 kHz) ->
//! phase-difference discriminator -> noise squelch -> 750 us de-emphasis ->
//! 300 Hz high-pass (strips CTCSS/DCS) -> 3 kHz low-pass -> volume.

use crate::AUDIO_RATE;
use crate::biquad::Biquad;
use std::f32::consts::{PI, TAU};

/// HackRF tunes this far below the wanted channel.
pub const RX_LO_OFFSET_HZ: i64 = 250_000;

/// Windowed-sinc (Hamming) low-pass FIR taps, unity DC gain.
pub fn design_lowpass(taps: usize, cutoff_hz: f32, sample_rate: f32) -> Vec<f32> {
    let fc = cutoff_hz / sample_rate;
    let m = (taps - 1) as f32;
    let mut h: Vec<f32> = (0..taps)
        .map(|n| {
            let x = n as f32 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * x).sin() / (PI * x)
            };
            sinc * (0.54 - 0.46 * (TAU * n as f32 / m).cos())
        })
        .collect();
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h
}

/// Decimating complex FIR with a doubled ring buffer (contiguous window, no wraparound).
struct DecimFir {
    taps: Vec<f32>,
    factor: usize,
    buf: Vec<(f32, f32)>,
    pos: usize,
    phase: usize,
}

impl DecimFir {
    fn new(taps: Vec<f32>, factor: usize) -> Self {
        let n = taps.len();
        Self {
            taps,
            factor,
            buf: vec![(0.0, 0.0); 2 * n],
            pos: 0,
            phase: 0,
        }
    }

    #[inline]
    fn push(&mut self, s: (f32, f32)) -> Option<(f32, f32)> {
        let n = self.taps.len();
        self.buf[self.pos] = s;
        self.buf[self.pos + n] = s;
        self.pos = (self.pos + 1) % n;
        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;
        // Oldest sample is at pos; taps are symmetric so direction does not matter.
        let w = &self.buf[self.pos..self.pos + n];
        let (mut i, mut q) = (0.0, 0.0);
        for (h, s) in self.taps.iter().zip(w) {
            i += h * s.0;
            q += h * s.1;
        }
        Some((i, q))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RxSettings {
    /// Linear speaker gain.
    pub volume: f32,
    /// Noise-squelch threshold, 0 = always open, 1 = tight. See [`FmReceiver::noise`].
    pub squelch: f32,
}

impl Default for RxSettings {
    fn default() -> Self {
        Self {
            volume: 0.5,
            squelch: 0.3,
        }
    }
}

pub struct FmReceiver {
    iq_rate: u32,
    /// NCO that shifts the channel from +offset to 0 Hz.
    nco: (f32, f32),
    nco_step: (f32, f32),
    nco_count: u32,
    stage1: DecimFir,
    stage2: DecimFir,
    last: (f32, f32),
    noise_hpf: [Biquad; 2],
    noise_env: f32,
    power_env: f32,
    open: bool,
    hang: u32,
    deemph: f32,
    deemph_alpha: f32,
    deemph_norm: f32,
    audio: [Biquad; 4],
    pub settings: RxSettings,
}

impl FmReceiver {
    /// `lo_offset_hz`: how far the wanted channel sits ABOVE the IQ centre.
    pub fn new(iq_rate: u32, lo_offset_hz: i64, settings: RxSettings) -> Self {
        assert_eq!(
            iq_rate,
            50 * AUDIO_RATE,
            "receiver chain is built for 2.4 Msps"
        );
        let fs1 = iq_rate as f32;
        let fs2 = fs1 / 10.0;
        let w = -TAU * lo_offset_hz as f32 / fs1;
        let fa = AUDIO_RATE as f32;
        // 750 us de-emphasis, normalized to unity at 1 kHz to mirror the transmitter.
        let deemph_alpha = (-1.0 / (750e-6 * fa)).exp();
        let w1k = TAU * 1_000.0 / fa;
        let deemph_norm =
            ((1.0 - deemph_alpha * w1k.cos()).powi(2) + (deemph_alpha * w1k.sin()).powi(2)).sqrt()
                / (1.0 - deemph_alpha);
        Self {
            iq_rate,
            nco: (1.0, 0.0),
            nco_step: (w.cos(), w.sin()),
            nco_count: 0,
            // Only needs to stop what would alias onto +/-12 kHz after /10.
            stage1: DecimFir::new(design_lowpass(41, 40_000.0, fs1), 10),
            // Channel filter: flat to ~6 kHz, well down by 12.5 kHz (adjacent channel).
            stage2: DecimFir::new(design_lowpass(151, 8_500.0, fs2), 5),
            last: (1.0, 0.0),
            noise_hpf: [
                Biquad::highpass(fa, 7_000.0, 0.541_196_1),
                Biquad::highpass(fa, 7_000.0, 1.306_563),
            ],
            noise_env: 1.0,
            power_env: 0.0,
            open: false,
            hang: 0,
            deemph: 0.0,
            deemph_alpha,
            deemph_norm,
            audio: [
                Biquad::highpass(fa, 300.0, 0.541_196_1),
                Biquad::highpass(fa, 300.0, 1.306_563),
                Biquad::lowpass(fa, 3_000.0, 0.541_196_1),
                Biquad::lowpass(fa, 3_000.0, 1.306_563),
            ],
            settings,
        }
    }

    /// Smoothed out-of-band discriminator noise, ~1.0 for no signal, ~0 for a full-quieting
    /// one. The squelch opens when this falls below `1 - settings.squelch`.
    pub fn noise(&self) -> f32 {
        self.noise_env
    }

    /// Smoothed channel power in dBFS (0 dBFS = full-scale cs8 carrier).
    pub fn power_dbfs(&self) -> f32 {
        10.0 * (self.power_env.max(1e-12)).log10()
    }

    pub fn squelch_open(&self) -> bool {
        self.open
    }

    pub fn iq_rate(&self) -> u32 {
        self.iq_rate
    }

    /// Demodulate cs8 `iq`, appending speaker audio at [`AUDIO_RATE`] to `out`.
    pub fn process(&mut self, iq: &[i8], out: &mut Vec<f32>) {
        // RMS (rad/sample) of the >7 kHz discriminator output on pure noise, measured with
        // probe_noise_levels; it is independent of noise amplitude. Normalizes noise() to 1.0.
        const NOISE_REF: f32 = 0.45;
        for c in iq.as_chunks::<2>().0 {
            let (i, q) = (f32::from(c[0]) / 128.0, f32::from(c[1]) / 128.0);
            let (nr, ni) = self.nco;
            let s = (i * nr - q * ni, i * ni + q * nr);
            let (sr, si) = self.nco_step;
            self.nco = (nr * sr - ni * si, nr * si + ni * sr);
            self.nco_count += 1;
            if self.nco_count == 4096 {
                // Renormalize so rounding error never grows the oscillator amplitude.
                let m = (self.nco.0 * self.nco.0 + self.nco.1 * self.nco.1).sqrt();
                self.nco = (self.nco.0 / m, self.nco.1 / m);
                self.nco_count = 0;
            }
            let Some(s1) = self.stage1.push(s) else {
                continue;
            };
            let Some((ci, cq)) = self.stage2.push(s1) else {
                continue;
            };

            let p = ci * ci + cq * cq;
            self.power_env += 0.002 * (p - self.power_env);
            // Discriminator: angle of z[n] * conj(z[n-1]), radians per audio sample.
            let (li, lq) = self.last;
            let d = (cq * li - ci * lq).atan2(ci * li + cq * lq);
            self.last = (ci, cq);

            let mut n = d;
            for f in &mut self.noise_hpf {
                n = f.process(n);
            }
            self.noise_env += 0.002 * ((n * n).sqrt() / NOISE_REF - self.noise_env);
            let want_open =
                self.settings.squelch <= 0.0 || self.noise_env < 1.0 - self.settings.squelch;
            // ~50 ms hysteresis-free hang so syllable gaps do not chop.
            if want_open {
                self.open = true;
                self.hang = AUDIO_RATE / 20;
            } else if self.hang > 0 {
                self.hang -= 1;
            } else {
                self.open = false;
            }

            // Scale so +/-5 kHz deviation is +/-1.0.
            let mut a = d * AUDIO_RATE as f32 / (TAU * 5_000.0);
            self.deemph = a + self.deemph_alpha * (self.deemph - a);
            a = self.deemph * self.deemph_norm;
            for f in &mut self.audio {
                a = f.process(a);
            }
            out.push(if self.open {
                (a * self.settings.volume * 4.0).clamp(-1.0, 1.0)
            } else {
                0.0
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm::{FmConfig, FmModulator, SubTone};

    const IQ_RATE: u32 = 2_400_000;

    fn tone(hz: f32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * AUDIO_RATE as f32) as usize)
            .map(|i| amp * (TAU * hz * i as f32 / AUDIO_RATE as f32).sin())
            .collect()
    }

    /// Shift cs8 IQ up by `hz` (as if received with the LO that far below the channel),
    /// optionally attenuated and with added noise. Deterministic LCG noise.
    fn channelize(iq: &[i8], hz: i64, gain: f32, noise: f32) -> Vec<i8> {
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let w = TAU * hz as f32 / IQ_RATE as f32;
        iq.as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .flat_map(|(n, c)| {
                let (s, co) = (
                    (w * (n % 48_000) as f32).sin(),
                    (w * (n % 48_000) as f32).cos(),
                );
                let (i, q) = (f32::from(c[0]) * gain, f32::from(c[1]) * gain);
                let o = [
                    i * co - q * s + noise * rnd(),
                    i * s + q * co + noise * rnd(),
                ];
                o.map(|v| v.round().clamp(-127.0, 127.0) as i8)
            })
            .collect()
    }

    fn goertzel(x: &[f32], hz: f32) -> f32 {
        let w = TAU * hz / AUDIO_RATE as f32;
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &v in x {
            let s = v + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() * 2.0 / x.len() as f32
    }

    fn receive(iq: &[i8], squelch: f32) -> (Vec<f32>, FmReceiver) {
        let mut rx = FmReceiver::new(
            IQ_RATE,
            RX_LO_OFFSET_HZ,
            RxSettings {
                volume: 0.25,
                squelch,
            },
        );
        let mut out = Vec::new();
        rx.process(iq, &mut out);
        (out, rx)
    }

    #[test]
    fn fir_has_unity_dc_gain_and_rejects_stopband() {
        let h = design_lowpass(151, 8_500.0, 240_000.0);
        assert!((h.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        let resp = |f: f32| {
            let w = TAU * f / 240_000.0;
            let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(r, i), (n, v)| {
                (r + v * (w * n as f32).cos(), i - v * (w * n as f32).sin())
            });
            20.0 * (re * re + im * im).sqrt().log10()
        };
        assert!(resp(5_000.0) > -1.0, "passband {:.1} dB", resp(5_000.0));
        assert!(
            resp(15_000.0) < -40.0,
            "adjacent channel {:.1} dB",
            resp(15_000.0)
        );
    }

    #[test]
    fn recovers_voice_and_strips_dcs() {
        let sub = SubTone::Dcs {
            code: 0o244,
            inverted: false,
        };
        let mut m = FmModulator::new(FmConfig::nbfm(IQ_RATE, Some(sub)));
        let mut iq = Vec::new();
        m.process(&tone(1_000.0, 1.0, 0.5), &mut iq);
        let (audio, rx) = receive(&channelize(&iq, RX_LO_OFFSET_HZ, 0.5, 2.0), 0.3);
        assert!(rx.squelch_open(), "noise {:.2}", rx.noise());
        let a = &audio[9_600..];
        let voice = goertzel(a, 1_000.0);
        assert!(voice > 0.1, "1 kHz at {voice:.3}");
        // DCS energy sits at ~67 Hz (alternating bits) and below; must be >30 dB under voice.
        for f in [67.2, 134.4] {
            let dcs = goertzel(a, f);
            assert!(dcs < voice / 31.6, "{f} Hz at {dcs:.4} vs voice {voice:.3}");
        }
    }

    #[test]
    fn noise_only_keeps_squelch_closed_and_output_silent() {
        let silence = vec![0i8; 2 * IQ_RATE as usize / 2];
        let (audio, rx) = receive(&channelize(&silence, 0, 0.0, 40.0), 0.3);
        assert!(!rx.squelch_open(), "noise {:.2}", rx.noise());
        assert!(audio[9_600..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn adjacent_channel_does_not_open_squelch() {
        // A strong carrier 25 kHz up, nothing on our channel but noise.
        let mut m = FmModulator::new(FmConfig::nbfm(IQ_RATE, None));
        let mut iq = Vec::new();
        m.process(&tone(1_000.0, 0.5, 0.5), &mut iq);
        let (_, rx) = receive(&channelize(&iq, RX_LO_OFFSET_HZ + 25_000, 0.5, 20.0), 0.3);
        assert!(!rx.squelch_open(), "noise {:.2}", rx.noise());
    }

    #[test]
    #[ignore = "calibration probe: cargo test -- --ignored --nocapture"]
    fn probe_noise_levels() {
        let silence = vec![0i8; IQ_RATE as usize];
        let mut m = FmModulator::new(FmConfig::nbfm(IQ_RATE, None));
        let mut sig = Vec::new();
        m.process(&tone(1_000.0, 0.5, 0.5), &mut sig);
        for noise in [5.0, 20.0, 40.0] {
            println!(
                "noise only (sigma {noise}): {:.3}",
                receive(&channelize(&silence, 0, 0.0, noise), 0.0).1.noise()
            );
        }
        for (g, n) in [
            (0.5, 2.0),
            (0.1, 20.0),
            (0.05, 20.0),
            (0.02, 20.0),
            (0.01, 20.0),
        ] {
            let (_, rx) = receive(&channelize(&sig, RX_LO_OFFSET_HZ, g, n), 0.0);
            println!(
                "signal gain {g} noise {n}: noise {:.3}, power {:.1} dBFS",
                rx.noise(),
                rx.power_dbfs()
            );
        }
    }
}
