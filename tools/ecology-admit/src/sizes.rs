//! sizes.rs — parse human byte sizes the way the box's tools speak them.

/// Parse `"512"`, `"30G"`, `"8 GiB"`, `"1.5g"` into bytes. Accepts the
/// bare SI prefixes (K/M/G/T, case-insensitive, optionally `i` and/or
/// `B`) with the *binary* multiplier (1 G = 1 GiB = 2^30) — the same
/// convention thor-admit's CLI and the tests' `--bytes 30G` use.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim().replace(' ', "");
    if t.is_empty() {
        return None;
    }
    let unit_start = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (number, unit) = t.split_at(unit_start);
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let unit = unit
        .trim_end_matches('b')
        .trim_end_matches('B')
        .to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" => 1u64,
        "k" | "ki" => 1 << 10,
        "m" | "mi" => 1 << 20,
        "g" | "gi" => 1 << 30,
        "t" | "ti" => 1 << 40,
        _ => return None,
    };
    Some((value * multiplier as f64) as u64)
}

/// Format bytes the way the daemon's logs and the bake-off table speak
/// them (GiB with one decimal).
pub fn fmt_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_boxs_vocabulary() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("30G"), Some(30 << 30));
        assert_eq!(parse_size("8 GiB"), Some(8 << 30));
        assert_eq!(parse_size("1.5g"), Some((1.5 * (1 << 30) as f64) as u64));
        assert_eq!(parse_size("2M"), Some(2 << 20));
        assert_eq!(parse_size("4K"), Some(4 << 10));
        assert_eq!(parse_size("1T"), Some(1 << 40));
    }

    #[test]
    fn refuses_nonsense() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size("-1G"), None);
        assert_eq!(parse_size("1Q"), None);
    }
}
