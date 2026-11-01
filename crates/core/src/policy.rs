//! Transmit authorization.
//!
//! One way to get on the air:
//!
//! 1. **Amateur** - the whole emission (carrier +/- half its occupied bandwidth) lies inside
//!    an allocation from [`crate::bandplan`] and respects that band's CW-only segment.
//!    Any power, amplifiers allowed.
//!
//! Everything else is refused. The `offband-test` feature adds a second way:
//!
//! 2. **Test into dummy load** - anything else the HackRF can physically tune, but only when
//!    the operator explicitly asserts the output goes into a dummy load / shielded test
//!    fixture with no amplification. The HackRF's internal RF amp is forced off and
//!    requesting it is refused rather than silently ignored.
//!
//! [`TxPermit`] has private fields and no public constructor, so hardware code that takes
//! `&TxPermit` cannot be driven without going through [`authorize`].

use crate::bandplan::{Band, band_for};
use crate::freq::fmt_mhz;
use std::fmt;

/// HackRF One transmit coverage.
pub const HACKRF_TX_MIN_HZ: u64 = 1_000_000;
pub const HACKRF_TX_MAX_HZ: u64 = 6_000_000_000;

/// Whether this build includes dummy-load test mode (the `offband-test` feature).
pub const OFFBAND_TEST_ENABLED: bool = cfg!(feature = "offband-test");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Emission {
    /// Narrowband FM voice (F3E). Phone, so it must stay out of CW-only segments.
    FmVoice { occupied_bw_hz: u32 },
    /// Unmodulated carrier - a test emission, allowed anywhere in a band (97.305(b)).
    Carrier,
}

impl fmt::Display for Emission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Emission::FmVoice { occupied_bw_hz } => {
                write!(f, "FM voice {:.1} kHz", f64::from(*occupied_bw_hz) / 1e3)
            }
            Emission::Carrier => write!(f, "carrier"),
        }
    }
}

impl Emission {
    pub fn occupied_bw_hz(&self) -> u32 {
        match *self {
            Emission::FmVoice { occupied_bw_hz } => occupied_bw_hz,
            // Allow for HackRF frequency error and phase noise skirts.
            Emission::Carrier => 1_000,
        }
    }

    fn is_phone(&self) -> bool {
        matches!(self, Emission::FmVoice { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TxRequest {
    pub freq_hz: u64,
    pub emission: Emission,
    /// HackRF's internal ~11-14 dB RF amplifier.
    pub internal_amp: bool,
    /// Operator asserts: output is into a dummy load / shielded fixture, no amplification.
    pub dummy_load_ack: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Authority {
    Amateur(&'static Band),
    TestIntoDummyLoad,
}

/// Proof that a specific transmission was authorized. Only [`authorize`] creates one.
#[derive(Debug, Clone, PartialEq)]
pub struct TxPermit {
    freq_hz: u64,
    emission: Emission,
    authority: Authority,
    internal_amp: bool,
    warnings: Vec<String>,
}

impl TxPermit {
    /// The only frequency this permit allows the radio to be tuned to.
    pub fn freq_hz(&self) -> u64 {
        self.freq_hz
    }
    pub fn emission(&self) -> Emission {
        self.emission
    }
    pub fn authority(&self) -> Authority {
        self.authority
    }
    pub fn internal_amp(&self) -> bool {
        self.internal_amp
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

impl fmt::Display for TxPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let who = match self.authority {
            Authority::Amateur(b) => format!("AMATEUR {} band", b.name),
            Authority::TestIntoDummyLoad => "TEST MODE - dummy load, no amplification".into(),
        };
        write!(
            f,
            "{} {} | {} | internal amp {}",
            fmt_mhz(self.freq_hz),
            self.emission,
            who,
            if self.internal_amp { "ON" } else { "off" }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    OutsideHardwareRange {
        freq_hz: u64,
    },
    /// Not (entirely) inside an amateur allocation, in a build without test mode.
    OutsideAmateurBands {
        freq_hz: u64,
        why: String,
    },
    /// Not (entirely) inside an amateur allocation and no dummy-load acknowledgement given.
    NeedsDummyLoad {
        freq_hz: u64,
        why: String,
    },
    /// Test mode was requested together with the internal amplifier.
    AmpInTestMode,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::OutsideHardwareRange { freq_hz } => write!(
                f,
                "{} is outside HackRF TX range ({} - {})",
                fmt_mhz(*freq_hz),
                fmt_mhz(HACKRF_TX_MIN_HZ),
                fmt_mhz(HACKRF_TX_MAX_HZ)
            ),
            Refusal::OutsideAmateurBands { freq_hz, why } => write!(
                f,
                "{} is not covered by amateur privileges ({why}); transmit refused",
                fmt_mhz(*freq_hz)
            ),
            Refusal::NeedsDummyLoad { freq_hz, why } => write!(
                f,
                "{} is not covered by your amateur privileges ({why}). Transmitting here is \
                 only allowed into a dummy load with no amplification: re-run with the \
                 dummy-load switch on.",
                fmt_mhz(*freq_hz)
            ),
            Refusal::AmpInTestMode => write!(
                f,
                "dummy-load test mode forbids amplification; turn the HackRF internal amp off"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Why (if at all) `req` falls outside amateur privileges. `Ok(band)` means fully covered.
fn amateur_coverage(req: &TxRequest) -> Result<&'static Band, String> {
    let half = u64::from(req.emission.occupied_bw_hz() / 2);
    let lo = req.freq_hz.saturating_sub(half);
    let hi = req.freq_hz + half;
    let Some(band) = band_for(req.freq_hz) else {
        return Err("no amateur allocation here".into());
    };
    if lo < band.lo_hz || hi > band.hi_hz {
        return Err(format!(
            "a {} Hz wide emission spills past the {} band edge",
            req.emission.occupied_bw_hz(),
            band.name
        ));
    }
    if let Some(cw_top) = band.cw_only_below_hz
        && req.emission.is_phone()
        && lo < cw_top
    {
        return Err(format!(
            "{} below {} is CW only",
            band.name,
            fmt_mhz(cw_top)
        ));
    }
    Ok(band)
}

pub fn authorize(req: &TxRequest) -> Result<TxPermit, Refusal> {
    if !(HACKRF_TX_MIN_HZ..=HACKRF_TX_MAX_HZ).contains(&req.freq_hz) {
        return Err(Refusal::OutsideHardwareRange {
            freq_hz: req.freq_hz,
        });
    }

    match amateur_coverage(req) {
        Ok(band) => {
            let mut warnings = Vec::new();
            if !band.note.is_empty() {
                warnings.push(format!("{}: {}", band.name, band.note));
            }
            warnings.push(
                "raw HackRF output is not spectrally clean: put a band filter after it \
                 before any external amplifier (97.307)"
                    .into(),
            );
            Ok(TxPermit {
                freq_hz: req.freq_hz,
                emission: req.emission,
                authority: Authority::Amateur(band),
                internal_amp: req.internal_amp,
                warnings,
            })
        }
        #[cfg(not(feature = "offband-test"))]
        Err(why) => Err(Refusal::OutsideAmateurBands {
            freq_hz: req.freq_hz,
            why,
        }),
        #[cfg(feature = "offband-test")]
        Err(why) if !req.dummy_load_ack => Err(Refusal::NeedsDummyLoad {
            freq_hz: req.freq_hz,
            why,
        }),
        #[cfg(feature = "offband-test")]
        Err(_) if req.internal_amp => Err(Refusal::AmpInTestMode),
        #[cfg(feature = "offband-test")]
        Err(why) => Ok(TxPermit {
            freq_hz: req.freq_hz,
            emission: req.emission,
            authority: Authority::TestIntoDummyLoad,
            internal_amp: false,
            warnings: vec![format!(
                "outside amateur privileges ({why}); HackRF output must go into a dummy \
                 load or shielded fixture, never an antenna or amplifier"
            )],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FM: Emission = Emission::FmVoice {
        occupied_bw_hz: 16_000,
    };

    fn req(freq_hz: u64) -> TxRequest {
        TxRequest {
            freq_hz,
            emission: FM,
            internal_amp: false,
            dummy_load_ack: false,
        }
    }

    #[test]
    fn repeater_inputs_are_amateur() {
        // Typical Boston-area repeater inputs on each target band.
        for f in [
            146_340_000,
            223_340_000,
            444_000_000,
            927_000_000,
            1_282_000_000,
        ] {
            let p = authorize(&TxRequest {
                internal_amp: true,
                ..req(f)
            })
            .unwrap();
            assert!(matches!(p.authority(), Authority::Amateur(_)), "{f}");
            assert!(p.internal_amp());
            assert_eq!(p.freq_hz(), f);
        }
    }

    #[cfg(not(feature = "offband-test"))]
    #[test]
    fn off_band_is_refused_even_with_the_dummy_load_switch() {
        // Aviation guard, HF (not encoded), and the excluded 219-220 MHz segment.
        for f in [121_500_000, 7_200_000, 219_500_000] {
            for dummy_load_ack in [false, true] {
                let r = authorize(&TxRequest {
                    dummy_load_ack,
                    ..req(f)
                });
                assert!(matches!(r, Err(Refusal::OutsideAmateurBands { .. })), "{f}");
            }
        }
    }

    #[cfg(feature = "offband-test")]
    #[test]
    fn out_of_band_requires_dummy_load_switch() {
        let air = 121_500_000;
        assert!(matches!(
            authorize(&req(air)),
            Err(Refusal::NeedsDummyLoad { .. })
        ));
        let p = authorize(&TxRequest {
            dummy_load_ack: true,
            ..req(air)
        })
        .unwrap();
        assert_eq!(p.authority(), Authority::TestIntoDummyLoad);
        assert!(!p.internal_amp());
    }

    #[cfg(feature = "offband-test")]
    #[test]
    fn amp_is_refused_in_test_mode() {
        let r = TxRequest {
            dummy_load_ack: true,
            internal_amp: true,
            ..req(162_550_000)
        };
        assert_eq!(authorize(&r), Err(Refusal::AmpInTestMode));
    }

    #[test]
    fn emission_must_fit_inside_band_edges() {
        assert!(authorize(&req(147_995_000)).is_err()); // 16 kHz wide spills past 148.000
        assert!(authorize(&req(147_990_000)).is_ok()); // exactly fits
        assert!(authorize(&req(420_004_000)).is_err());
    }

    #[test]
    fn phone_stays_out_of_cw_segment_but_carrier_may_enter() {
        assert!(authorize(&req(144_050_000)).is_err());
        assert!(authorize(&req(144_105_000)).is_err()); // lower sideband edge at 144.097
        assert!(authorize(&req(144_110_000)).is_ok());
        let carrier = TxRequest {
            emission: Emission::Carrier,
            ..req(144_050_000)
        };
        assert!(authorize(&carrier).is_ok());
    }

    #[test]
    fn hardware_limits_are_absolute() {
        let r = TxRequest {
            dummy_load_ack: true,
            ..req(500_000)
        };
        assert!(matches!(
            authorize(&r),
            Err(Refusal::OutsideHardwareRange { .. })
        ));
        let r = TxRequest {
            dummy_load_ack: true,
            ..req(7_000_000_000)
        };
        assert!(matches!(
            authorize(&r),
            Err(Refusal::OutsideHardwareRange { .. })
        ));
    }

    #[test]
    fn seventy_cm_carries_the_us7_power_warning() {
        let p = authorize(&req(446_000_000)).unwrap();
        assert!(p.warnings().iter().any(|w| w.contains("US7")));
    }
}
