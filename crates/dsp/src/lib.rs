//! Audio sources and a narrowband FM modulator that produces HackRF-native cs8 IQ
//! (interleaved signed 8-bit I/Q).

pub mod biquad;
pub mod dcs;
pub mod fm;
pub mod rx;
pub mod sources;

/// Audio sample rate used throughout the audio chain.
pub const AUDIO_RATE: u32 = 48_000;
