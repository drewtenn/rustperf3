//! Parse iperf3-style bandwidth strings for `-b`.
//! Accepts integer bits-per-second with optional K/M/G suffix (case-
//! insensitive). Returns bps. "0" (or "") means unlimited.

pub fn parse(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Ok(0);
    }
    let (num_part, mult): (&str, u64) = match s.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&s[..s.len() - 1], 1_000),
        Some(b'm') | Some(b'M') => (&s[..s.len() - 1], 1_000_000),
        Some(b'g') | Some(b'G') => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    let n: u64 = num_part
        .parse()
        .map_err(|e| format!("invalid bandwidth '{}': {}", s, e))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("bandwidth '{}' overflows u64", s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_unlimited() {
        assert_eq!(parse("0").unwrap(), 0);
        assert_eq!(parse("").unwrap(), 0);
    }

    #[test]
    fn integer_bps_passthrough() {
        assert_eq!(parse("1000").unwrap(), 1_000);
    }

    #[test]
    fn k_suffix_is_thousand_bits() {
        assert_eq!(parse("1k").unwrap(), 1_000);
        assert_eq!(parse("250K").unwrap(), 250_000);
    }

    #[test]
    fn m_suffix_is_million_bits() {
        assert_eq!(parse("1M").unwrap(), 1_000_000);
        assert_eq!(parse("100m").unwrap(), 100_000_000);
    }

    #[test]
    fn g_suffix_is_billion_bits() {
        assert_eq!(parse("2G").unwrap(), 2_000_000_000);
    }

    #[test]
    fn invalid_input_errors() {
        assert!(parse("abc").is_err());
        assert!(parse("1.5M").is_err());
        assert!(parse("M").is_err());
    }

    #[test]
    fn overflow_errors() {
        assert!(parse("99999999999G").is_err());
    }
}
