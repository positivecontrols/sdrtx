//! Mono audio sources at [`AUDIO_RATE`].

use crate::AUDIO_RATE;
use std::path::Path;

pub trait AudioSource: Send {
    /// Fill `buf` with samples; returns how many were written. 0 means end of stream.
    fn read(&mut self, buf: &mut [f32]) -> usize;
}

/// Endless sine tone.
pub struct Tone {
    step: f64,
    phase: f64,
    amplitude: f32,
}

impl Tone {
    pub fn new(hz: f32, amplitude: f32) -> Self {
        Self {
            step: std::f64::consts::TAU * f64::from(hz) / f64::from(AUDIO_RATE),
            phase: 0.0,
            amplitude,
        }
    }
}

impl AudioSource for Tone {
    fn read(&mut self, buf: &mut [f32]) -> usize {
        for s in buf.iter_mut() {
            *s = self.amplitude * self.phase.sin() as f32;
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
        }
        buf.len()
    }
}

/// Endless silence (an unmodulated carrier, or CTCSS only).
pub struct Silence;

impl AudioSource for Silence {
    fn read(&mut self, buf: &mut [f32]) -> usize {
        buf.fill(0.0);
        buf.len()
    }
}

/// Stops another source after a fixed number of samples.
pub struct Take<S> {
    inner: S,
    remaining: usize,
}

impl<S: AudioSource> Take<S> {
    pub fn new(inner: S, secs: f64) -> Self {
        Self {
            inner,
            remaining: (secs * f64::from(AUDIO_RATE)) as usize,
        }
    }
}

impl<S: AudioSource> AudioSource for Take<S> {
    fn read(&mut self, buf: &mut [f32]) -> usize {
        let n = buf.len().min(self.remaining);
        let got = self.inner.read(&mut buf[..n]);
        self.remaining -= got;
        got
    }
}

/// A WAV file, downmixed to mono and resampled to [`AUDIO_RATE`], held in memory.
pub struct WavFile {
    samples: Vec<f32>,
    pos: usize,
}

impl WavFile {
    pub fn open(path: &Path) -> Result<Self, hound::Error> {
        let mut r = hound::WavReader::open(path)?;
        let spec = r.spec();
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let full = (1i64 << (spec.bits_per_sample - 1)) as f32;
                r.samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / full))
                    .collect::<Result<_, _>>()?
            }
        };
        let ch = usize::from(spec.channels);
        let mono: Vec<f32> = interleaved
            .chunks_exact(ch)
            .map(|c| c.iter().sum::<f32>() / ch as f32)
            .collect();
        Ok(Self {
            samples: resample_linear(&mono, spec.sample_rate, AUDIO_RATE),
            pos: 0,
        })
    }

    pub fn duration_secs(&self) -> f64 {
        self.samples.len() as f64 / f64::from(AUDIO_RATE)
    }
}

impl AudioSource for WavFile {
    fn read(&mut self, buf: &mut [f32]) -> usize {
        let n = buf.len().min(self.samples.len() - self.pos);
        buf[..n].copy_from_slice(&self.samples[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

/// Linear-interpolation resampler. Adequate for speech going through a 3 kHz low-pass;
/// not for hi-fi.
fn resample_linear(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    let n = ((x.len() - 1) as f64 / step) as usize + 1;
    (0..n)
        .map(|i| {
            let t = i as f64 * step;
            let j = t as usize;
            let f = (t - j as f64) as f32;
            let b = x.get(j + 1).copied().unwrap_or(x[j]);
            x[j] + (b - x[j]) * f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_stops_after_duration() {
        let mut s = Take::new(Silence, 0.1);
        let mut buf = [0.0; 1000];
        let mut total = 0;
        loop {
            let n = s.read(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 4_800);
    }

    #[test]
    fn resample_preserves_duration() {
        let x = vec![0.5; 8_000];
        let y = resample_linear(&x, 8_000, 48_000);
        assert!((y.len() as i64 - 47_995).abs() < 10);
        assert!(y.iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }
}
