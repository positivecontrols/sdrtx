//! Microphone and speaker via cpal, both mono f32 at [`AUDIO_RATE`].

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, Stream, StreamConfig};
use sdrtx_dsp::AUDIO_RATE;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// A bounded FIFO of mono samples shared with an audio callback. When full, the oldest
/// samples are discarded, so latency can never grow past `cap`.
pub struct AudioQueue {
    q: Mutex<VecDeque<f32>>,
    cap: usize,
    /// Peak absolute level of the most recent callback, f32 bits.
    peak: AtomicU32,
}

impl AudioQueue {
    pub fn new(cap_secs: f32) -> Arc<Self> {
        let cap = (cap_secs * AUDIO_RATE as f32) as usize;
        Arc::new(Self {
            q: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
            peak: AtomicU32::new(0),
        })
    }

    pub fn push(&self, samples: impl IntoIterator<Item = f32>) {
        let mut q = self.q.lock().unwrap();
        for s in samples {
            if q.len() == self.cap {
                q.pop_front();
            }
            q.push_back(s);
        }
    }

    /// Fill `out` from the front of the queue; returns how many real samples were available.
    pub fn pop_into(&self, out: &mut [f32]) -> usize {
        let mut q = self.q.lock().unwrap();
        let n = out.len().min(q.len());
        for (o, s) in out.iter_mut().zip(q.drain(..n)) {
            *o = s;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.q.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.q.lock().unwrap().clear();
    }

    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    fn set_peak(&self, p: f32) {
        self.peak.store(p.to_bits(), Ordering::Relaxed);
    }
}

/// An audio device. `id` is cpal's unique, persistable device id (e.g.
/// `alsa:plughw:CARD=Mini,DEV=0`); display names alone are NOT unique - ALSA exposes one
/// device per PCM (hw, plughw, dsnoop, ...) and they all share the card's description.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioDevice {
    pub id: String,
    pub label: String,
}

fn list(input: bool) -> Vec<(Device, AudioDevice)> {
    let host = cpal::default_host();
    let devs = if input {
        host.input_devices()
    } else {
        host.output_devices()
    };
    let Ok(devs) = devs else { return Vec::new() };
    devs.filter_map(|d| {
        let id = d.id().ok()?.to_string();
        let pcm = id
            .split_once(':')
            .map_or(id.as_str(), |(_, rest)| rest)
            .to_string();
        let label = format!("{d} ({pcm})");
        Some((d, AudioDevice { id, label }))
    })
    .collect()
}

pub fn input_devices() -> Vec<AudioDevice> {
    list(true).into_iter().map(|(_, a)| a).collect()
}

pub fn output_devices() -> Vec<AudioDevice> {
    list(false).into_iter().map(|(_, a)| a).collect()
}

fn find(id: Option<&str>, input: bool) -> Result<Device, String> {
    let host = cpal::default_host();
    match id {
        Some(id) => list(input)
            .into_iter()
            .find(|(_, a)| a.id == id)
            .map(|(d, _)| d)
            .ok_or_else(|| format!("audio device {id:?} not found")),
        None if input => host
            .default_input_device()
            .ok_or_else(|| "no microphone found".into()),
        None => host
            .default_output_device()
            .ok_or_else(|| "no speaker found".into()),
    }
}

/// When the system default is unusable (on a stock Pi the ALSA `default` PCM has no capture
/// side), fall back to a resampling `plughw` device, preferring anything that is not HDMI.
fn fallback(input: bool) -> Option<Device> {
    let mut plug: Vec<_> = list(input)
        .into_iter()
        .filter(|(_, a)| a.id.contains(":plughw:"))
        .collect();
    plug.sort_by_key(|(_, a)| a.id.contains("hdmi"));
    plug.into_iter().next().map(|(d, _)| d)
}

/// Open `id` (or the default, falling back per [`fallback`]) with `open`.
fn open_with(
    id: Option<&str>,
    input: bool,
    open: impl Fn(Device) -> Result<Stream, String>,
) -> Result<Stream, String> {
    match find(id, input).and_then(&open) {
        Err(e) if id.is_none() => fallback(input).ok_or(e).and_then(&open),
        r => r,
    }
}

fn config(dev: &Device, input: bool) -> Result<StreamConfig, String> {
    let def = if input {
        dev.default_input_config()
    } else {
        dev.default_output_config()
    }
    .map_err(|e| e.to_string())?;
    Ok(StreamConfig {
        channels: def.channels(),
        sample_rate: AUDIO_RATE as SampleRate,
        buffer_size: BufferSize::Default,
    })
}

/// Live microphone: downmixes to mono, applies no gain, pushes into `queue`.
pub fn open_mic(id: Option<&str>, queue: Arc<AudioQueue>) -> Result<Stream, String> {
    open_with(id, true, |dev| open_mic_on(dev, queue.clone()))
}

fn open_mic_on(dev: Device, queue: Arc<AudioQueue>) -> Result<Stream, String> {
    let cfg = config(&dev, true)?;
    let ch = usize::from(cfg.channels);
    let stream = dev
        .build_input_stream::<f32, _, _>(
            cfg,
            move |data, _| {
                let mono = data
                    .chunks_exact(ch)
                    .map(|c| c.iter().sum::<f32>() / ch as f32);
                let peak = data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                queue.set_peak(peak);
                queue.push(mono);
            },
            |e| eprintln!("mic: {e}"),
            None,
        )
        .map_err(|e| format!("{dev}: {e}"))?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Live speaker: plays mono samples from `queue` on every channel; silence on underrun.
pub fn open_speaker(id: Option<&str>, queue: Arc<AudioQueue>) -> Result<Stream, String> {
    open_with(id, false, |dev| open_speaker_on(dev, queue.clone()))
}

fn open_speaker_on(dev: Device, queue: Arc<AudioQueue>) -> Result<Stream, String> {
    let cfg = config(&dev, false)?;
    let ch = usize::from(cfg.channels);
    let mut mono = Vec::new();
    let stream = dev
        .build_output_stream::<f32, _, _>(
            cfg,
            move |data, _| {
                mono.resize(data.len() / ch, 0.0);
                let n = queue.pop_into(&mut mono);
                mono[n..].fill(0.0);
                for (frame, s) in data.chunks_exact_mut(ch).zip(&mono) {
                    frame.fill(*s);
                }
                queue.set_peak(mono.iter().fold(0.0f32, |m, v| m.max(v.abs())));
            },
            |e| eprintln!("speaker: {e}"),
            None,
        )
        .map_err(|e| format!("{dev}: {e}"))?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}
