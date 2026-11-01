//! US amateur allocations a General-class licensee may transmit on, limited to what a
//! HackRF One can reach (1 MHz - 6 GHz).
//!
//! Above 50 MHz, General privileges are identical to Extra, so every VHF/UHF/microwave
//! amateur allocation is included in full. HF General segments are NOT encoded yet (their
//! phone/CW sub-bands are fiddly); an HF frequency is therefore refused rather than being
//! authorized by mistake.
//!
//! Deliberately excluded, so they are also refused:
//! - 219-220 MHz: fixed point-to-point digital message forwarding only, with prior
//!   notification (47 CFR 97.303(e)); FM voice is not permitted there.
//! - 3.3-3.5 GHz: amateur status changed by the FCC's 2020 3 GHz proceeding; add it back
//!   only after checking the current rules.

/// One contiguous amateur allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub name: &'static str,
    pub lo_hz: u64,
    pub hi_hz: u64,
    /// Bottom-of-band segment restricted to CW (47 CFR 97.305(c)). Phone must sit above it.
    pub cw_only_below_hz: Option<u64>,
    /// Power ceiling that applies in some locations (see `note`), if any.
    pub max_pep_w: Option<f64>,
    /// Operator-facing reminder printed with every permit on this band.
    pub note: &'static str,
}

impl Band {
    pub fn contains(&self, hz: u64) -> bool {
        (self.lo_hz..=self.hi_hz).contains(&hz)
    }
}

const MHZ: u64 = 1_000_000;

pub static US_GENERAL: &[Band] = &[
    Band {
        name: "6m",
        lo_hz: 50 * MHZ,
        hi_hz: 54 * MHZ,
        cw_only_below_hz: Some(50_100_000),
        max_pep_w: None,
        note: "",
    },
    Band {
        name: "2m",
        lo_hz: 144 * MHZ,
        hi_hz: 148 * MHZ,
        cw_only_below_hz: Some(144_100_000),
        max_pep_w: None,
        note: "",
    },
    Band {
        name: "1.25m",
        lo_hz: 222 * MHZ,
        hi_hz: 225 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "",
    },
    Band {
        name: "70cm",
        lo_hz: 420 * MHZ,
        hi_hz: 450 * MHZ,
        cw_only_below_hz: None,
        // Footnote US7: within 160 km of Otis AFB / Cape Cod SFS (PAVE PAWS) amateur
        // stations are limited to 50 W PEP. The footnote lists other areas too.
        max_pep_w: Some(50.0),
        note: "50 W PEP max in some areas, including near Cape Cod: footnote US7",
    },
    Band {
        name: "33cm",
        lo_hz: 902 * MHZ,
        hi_hz: 928 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "shared with ISM/Part 15 devices; amateur is secondary",
    },
    Band {
        name: "23cm",
        lo_hz: 1240 * MHZ,
        hi_hz: 1300 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "secondary to radiolocation and GNSS (Galileo E6 / GLONASS L3); keep it clean",
    },
    Band {
        name: "13cm-low",
        lo_hz: 2300 * MHZ,
        hi_hz: 2310 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "",
    },
    Band {
        name: "13cm-high",
        lo_hz: 2390 * MHZ,
        hi_hz: 2450 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "overlaps 2.4 GHz WiFi/ISM",
    },
    Band {
        name: "5cm",
        lo_hz: 5650 * MHZ,
        hi_hz: 5925 * MHZ,
        cw_only_below_hz: None,
        max_pep_w: None,
        note: "overlaps 5.8 GHz WiFi/ISM",
    },
];

/// The amateur band containing `hz`, if any.
pub fn band_for(hz: u64) -> Option<&'static Band> {
    US_GENERAL.iter().find(|b| b.contains(hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_are_sorted_and_disjoint() {
        for w in US_GENERAL.windows(2) {
            assert!(w[0].lo_hz < w[0].hi_hz);
            assert!(
                w[0].hi_hz < w[1].lo_hz,
                "{} overlaps {}",
                w[0].name,
                w[1].name
            );
        }
    }

    #[test]
    fn finds_the_repeater_bands() {
        assert_eq!(band_for(146_520_000).unwrap().name, "2m");
        assert_eq!(band_for(223_500_000).unwrap().name, "1.25m");
        assert_eq!(band_for(446_000_000).unwrap().name, "70cm");
        assert_eq!(band_for(927_000_000).unwrap().name, "33cm");
        assert_eq!(band_for(1_294_500_000).unwrap().name, "23cm");
        assert!(band_for(121_500_000).is_none()); // aviation guard
        assert!(band_for(219_500_000).is_none()); // excluded on purpose
    }
}
