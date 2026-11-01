//! Digital-Coded Squelch (DCS / DPL) word generation.
//!
//! A DCS word is a 23-bit Golay (23,12) codeword: the 9-bit code (three octal digits) least
//! significant bit first, then the fixed bits 0, 0, 1, then 11 check bits. Bit 0 is sent
//! first, NRZ at [`DCS_BAUD`], repeating with no framing. In normal polarity a 1 is
//! positive deviation (carrier above the tuned frequency); inverted polarity sends the
//! complement. Check bits make the word, read as a polynomial with bit i the coefficient of
//! x^i, a multiple of the generator x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1.

/// DCS bit rate.
pub const DCS_BAUD: f64 = 134.4;

const GENERATOR: u32 = 0b1100_0111_0101; // x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1
const MASK_23: u32 = (1 << 23) - 1;

/// GF(2) polynomial remainder of `v` modulo [`GENERATOR`].
fn poly_mod(mut v: u32) -> u32 {
    for bit in (11..23).rev() {
        if v & (1 << bit) != 0 {
            v ^= GENERATOR << (bit - 11);
        }
    }
    v
}

/// The on-air 23-bit word for `code` (0..=0o777), bit 0 sent first.
pub fn dcs_word(code: u16, inverted: bool) -> u32 {
    assert!(code <= 0o777, "DCS code out of range: {code:o}");
    let data = u32::from(code) | 1 << 11;
    // data + x^12 * p must be divisible by g, so x^12 * p == data (mod g). Searching all
    // 2^11 check patterns is trivially cheap and obviously correct.
    let parity = (0..1u32 << 11)
        .find(|p| poly_mod(data ^ (p << 12)) == 0)
        .expect("every Golay data word has exactly one check pattern");
    let word = data | parity << 12;
    if inverted { !word & MASK_23 } else { word }
}

/// Parse `244`, `244N`, `244I`, `D244N`, `d023i`. Digits are octal. Returns (code, inverted).
pub fn parse_dcs(s: &str) -> Result<(u16, bool), String> {
    let t = s.trim().trim_start_matches(['D', 'd']);
    let (digits, inverted) = match t.chars().last() {
        Some('N' | 'n' | 'P' | 'p') => (&t[..t.len() - 1], false),
        Some('I' | 'i') => (&t[..t.len() - 1], true),
        _ => (t, false),
    };
    if digits.len() != 3 {
        return Err(format!("DCS code must be three octal digits: {s}"));
    }
    let code =
        u16::from_str_radix(digits, 8).map_err(|_| format!("DCS digits must be 0-7: {s}"))?;
    Ok((code, inverted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_published_vectors() {
        // KB8ZQZ DPL page (023 shown MSB-first as 11101100011-100-000/010/011) and dsd-neo.
        assert_eq!(dcs_word(0o023, false), 0x76_3813);
        assert_eq!(dcs_word(0o023, true), 0x09_C7EC);
        assert_eq!(dcs_word(0o000, false), 0x63_A800);
    }

    #[test]
    fn every_word_is_a_codeword_with_the_signature() {
        for code in 0..=0o777 {
            let w = dcs_word(code, false);
            assert_eq!(poly_mod(w), 0);
            assert_eq!(w & 0xFFF, u32::from(code) | 0x800);
        }
    }

    #[test]
    fn inverted_023_is_a_rotation_of_normal_047() {
        // Classic DCS alias pair: -023 and +047 are the same cyclic bit pattern.
        let a = dcs_word(0o023, true);
        let b = dcs_word(0o047, false);
        assert!((0..23).any(|r| ((a >> r) | (a << (23 - r))) & MASK_23 == b));
    }

    #[test]
    fn parses_notations() {
        assert_eq!(parse_dcs("244").unwrap(), (0o244, false));
        assert_eq!(parse_dcs("D244N").unwrap(), (0o244, false));
        assert_eq!(parse_dcs("d023i").unwrap(), (0o023, true));
        assert!(parse_dcs("248").is_err());
        assert!(parse_dcs("24").is_err());
    }
}
