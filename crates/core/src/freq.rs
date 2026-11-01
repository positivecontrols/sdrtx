//! Human-friendly frequency parsing and formatting.

/// Parse an absolute frequency such as `146.52M`, `146520k`, `1.2965G` or `146520000`.
///
/// A bare number containing a decimal point is taken as MHz (`146.52` -> 146.52 MHz),
/// since that is how hams write frequencies; a bare integer is taken as Hz.
pub fn parse_freq(s: &str) -> Result<u64, String> {
    let hz = parse_scaled(s, true)?;
    if hz < 0 {
        return Err(format!("frequency must be positive: {s}"));
    }
    Ok(hz as u64)
}

/// Parse a signed offset such as `+600k`, `-5M`, `-600000`. A bare integer is Hz.
pub fn parse_offset(s: &str) -> Result<i64, String> {
    parse_scaled(s, false)
}

fn parse_scaled(s: &str, bare_decimal_is_mhz: bool) -> Result<i64, String> {
    let t = s.trim();
    let (num, mult) = match t.chars().last() {
        Some('k' | 'K') => (&t[..t.len() - 1], 1e3),
        Some('M') => (&t[..t.len() - 1], 1e6),
        Some('G' | 'g') => (&t[..t.len() - 1], 1e9),
        _ if bare_decimal_is_mhz && t.contains('.') => (t, 1e6),
        _ => (t, 1.0),
    };
    let v: f64 = num
        .trim_start_matches('+')
        .parse()
        .map_err(|_| format!("cannot parse frequency: {s}"))?;
    Ok((v * mult).round() as i64)
}

/// Format Hz as MHz with enough digits to show 10 Hz resolution, e.g. `146.520000 MHz`.
pub fn fmt_mhz(hz: u64) -> String {
    format!("{:.6} MHz", hz as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_suffixes_and_bare_values() {
        assert_eq!(parse_freq("146.52M").unwrap(), 146_520_000);
        assert_eq!(parse_freq("146.52").unwrap(), 146_520_000);
        assert_eq!(parse_freq("146520k").unwrap(), 146_520_000);
        assert_eq!(parse_freq("1.2965G").unwrap(), 1_296_500_000);
        assert_eq!(parse_freq("146520000").unwrap(), 146_520_000);
        assert_eq!(parse_offset("+600k").unwrap(), 600_000);
        assert_eq!(parse_offset("-5M").unwrap(), -5_000_000);
        assert!(parse_freq("-1M").is_err());
        assert!(parse_freq("abc").is_err());
    }
}
