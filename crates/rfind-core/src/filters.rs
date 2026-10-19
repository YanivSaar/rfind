//! Shared size/time filter parsers.
//!
//! Grammars (both base-1024):
//!
//! - Size: `[+-]?N(c|k|K|M|G|T)[B|iB]?`
//!   `+` = larger, `-` = smaller, bare = exact.
//!   `c` = byte, `k`/`K` = KiB, `M` = MiB, `G` = GiB, `T` = TiB.
//!   A trailing `B` or `iB` is accepted (`10MiB`, `500KB`).
//!   Fractional values allowed (`1.5G`).
//!
//! - Time: `[+-]?N(s|m|h|d)`
//!   `+` = older than N, `-` = newer than N, bare = exact.
//!   `s` = second, `m` = minute, `h` = hour, `d` = day.
//!
//! Both parsers accept daviddl9's and YanivSaar's forms. The CLI maps the
//! parsed value into a `(min, max)` byte bound or a `(from_sec, to_sec)`
//! epoch window; the index and the walk predicate consume those.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeOp {
    /// `+N`: entry's mtime is strictly older than `now - N`.
    Older,
    /// `-N`: entry's mtime is strictly newer than `now - N`.
    Newer,
    /// `N`: entry's mtime equals `now - N` (±1 second window).
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeFilter {
    pub op: TimeOp,
    pub secs: i64,
}

impl TimeFilter {
    /// Convert the filter into an inclusive `(from_sec, to_sec)` epoch
    /// window relative to `now`. `Exact` becomes a 3-second window centred
    /// on `now - secs`.
    pub fn to_range(self, now: i64) -> (i64, i64) {
        match self.op {
            TimeOp::Older => (i64::MIN, now - self.secs),
            TimeOp::Newer => (now - self.secs, i64::MAX),
            TimeOp::Exact => (now - self.secs - 1, now - self.secs + 1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeOp {
    /// `+N`: file size is at least N bytes.
    Larger,
    /// `-N`: file size is at most N bytes.
    Smaller,
    /// `N`: file size equals N bytes.
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeFilter {
    pub op: SizeOp,
    pub bytes: u64,
}

impl SizeFilter {
    /// Translate the filter into `(min_bytes, max_bytes)` inclusive bounds.
    pub fn to_bounds(self) -> (Option<u64>, Option<u64>) {
        match self.op {
            SizeOp::Larger => (Some(self.bytes), None),
            SizeOp::Smaller => (None, Some(self.bytes)),
            SizeOp::Exact => (Some(self.bytes), Some(self.bytes)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a `[+-]?N[smhd]` time spec.
pub fn parse_time(s: &str) -> Result<TimeFilter, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty time filter".into());
    }
    let (op, rest) = split_prefix(t, "time filter")?;
    let op = match op {
        Some('+') => TimeOp::Older,
        Some('-') => TimeOp::Newer,
        None => TimeOp::Exact,
        _ => unreachable!(),
    };
    let suffix = rest
        .chars()
        .next_back()
        .ok_or_else(|| err_time(s, "missing number"))?;
    let unit = match suffix {
        's' | 'S' => 1_i64,
        'm' | 'M' => 60,
        'h' | 'H' => 3600,
        'd' | 'D' => 86_400,
        _ => return Err(err_time(s, "unit must be s, m, h, or d")),
    };
    let num_part = &rest[..rest.len() - suffix.len_utf8()];
    let n: i64 = num_part
        .parse()
        .map_err(|_| err_time(s, "number must be a non-negative integer"))?;
    if n < 0 {
        return Err(err_time(s, "number must be non-negative"));
    }
    let secs = n
        .checked_mul(unit)
        .ok_or_else(|| err_time(s, "value overflows i64 seconds"))?;
    Ok(TimeFilter { op, secs })
}

/// Parse a `[+-]?N[ckKmMgGtT][B|iB]?` size spec. Unit letters are
/// case-insensitive. `c` is a byte; everything else is base-1024.
pub fn parse_size(s: &str) -> Result<SizeFilter, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("empty size filter".into());
    }
    let (op, after_prefix) = split_prefix(raw, "size filter")?;
    let op = match op {
        Some('+') => SizeOp::Larger,
        Some('-') => SizeOp::Smaller,
        None => SizeOp::Exact,
        _ => unreachable!(),
    };

    // Strip an optional trailing "iB" or "B" (case-insensitive).
    let mut body = after_prefix;
    if let Some(stripped) = strip_suffix_icase(body, "iB") {
        body = stripped;
    } else if let Some(stripped) = strip_suffix_icase(body, "B") {
        body = stripped;
    }
    if body.is_empty() {
        return Err(err_size(s, "missing number"));
    }

    let last = body.chars().next_back().unwrap();
    let (num_part, mult) = if last.is_ascii_digit() || last == '.' {
        (body, 1_u64)
    } else {
        let m = unit_byte(last);
        if m == 0 {
            return Err(err_size(s, "unit must be c, k, m, g, or t"));
        }
        (&body[..body.len() - last.len_utf8()], m)
    };

    let n: f64 = num_part
        .parse()
        .map_err(|_| err_size(s, "number must be a non-negative integer or decimal"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(err_size(s, "number must be finite and non-negative"));
    }
    let bytes = n * mult as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(err_size(s, "value overflows u64 bytes"));
    }
    Ok(SizeFilter {
        op,
        bytes: bytes as u64,
    })
}

/// Bytes per size-unit character. Case-insensitive; 0 for non-unit chars.
/// `c` is a byte (daviddl9); `k/m/g/t` are KiB/MiB/GiB/TiB.
fn unit_byte(c: char) -> u64 {
    match c {
        'c' | 'C' => 1,
        'k' | 'K' => 1 << 10,
        'm' | 'M' => 1 << 20,
        'g' | 'G' => 1 << 30,
        't' | 'T' => 1 << 40,
        _ => 0,
    }
}

/// Peel an optional leading `+` or `-`. Returns the unaltered string if
/// absent, or an error if a different non-digit prefix appears.
fn split_prefix<'a>(s: &'a str, label: &str) -> Result<(Option<char>, &'a str), String> {
    let mut chars = s.chars();
    let first = chars
        .next()
        .ok_or_else(|| format!("empty {}", label))?;
    match first {
        '+' | '-' => Ok((Some(first), &s[1..])),
        c if c.is_ascii_digit() || c == '.' => Ok((None, s)),
        _ => Err(format!(
            "invalid {} '{}': expected a number optionally prefixed with + or -",
            label, s
        )),
    }
}

/// Strip a trailing ASCII-only suffix, case-insensitively. Returns None
/// unless every byte of the tail is ASCII (so the split point is always a
/// char boundary — otherwise `&s[..cut]` would panic on multibyte tails).
fn strip_suffix_icase<'a>(s: &'a str, suf: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    let sb = suf.as_bytes();
    if bytes.len() < sb.len() {
        return None;
    }
    let cut = bytes.len() - sb.len();
    let tail = &bytes[cut..];
    if tail.iter().any(|b| !b.is_ascii()) {
        return None;
    }
    if tail.eq_ignore_ascii_case(sb) {
        Some(&s[..cut])
    } else {
        None
    }
}

fn err_time(s: &str, why: &str) -> String {
    format!("invalid time filter '{s}': {why} (try +1d, -2h, 30m, 45s)")
}

fn err_size(s: &str, why: &str) -> String {
    format!("invalid size filter '{s}': {why} (try +100M, -1k, 500c, 1.5G)")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_time ----

    #[test]
    fn time_older_days() {
        let r = parse_time("+7d").unwrap();
        assert_eq!(r.op, TimeOp::Older);
        assert_eq!(r.secs, 7 * 86_400);
    }

    #[test]
    fn time_newer_minutes() {
        let r = parse_time("-15m").unwrap();
        assert_eq!(r.op, TimeOp::Newer);
        assert_eq!(r.secs, 15 * 60);
    }

    #[test]
    fn time_exact_hours() {
        let r = parse_time("3h").unwrap();
        assert_eq!(r.op, TimeOp::Exact);
        assert_eq!(r.secs, 3 * 3600);
    }

    #[test]
    fn time_seconds() {
        assert_eq!(parse_time("+90s").unwrap().secs, 90);
    }

    #[test]
    fn time_rejects_bad_unit() {
        assert!(parse_time("1w").is_err());
    }

    #[test]
    fn time_rejects_missing_unit() {
        assert!(parse_time("42").is_err());
    }

    #[test]
    fn time_rejects_empty() {
        assert!(parse_time("").is_err());
    }

    #[test]
    fn time_rejects_negative() {
        assert!(parse_time("--3d").is_err());
    }

    #[test]
    fn time_rejects_multibyte_tail() {
        // Regression against daviddl9's byte-indexed slice bug.
        assert!(parse_time("1é").is_err());
    }

    #[test]
    fn time_to_range_older() {
        let (lo, hi) = TimeFilter {
            op: TimeOp::Older,
            secs: 100,
        }
        .to_range(1_000_000);
        assert_eq!(lo, i64::MIN);
        assert_eq!(hi, 999_900);
    }

    #[test]
    fn time_to_range_newer() {
        let (lo, hi) = TimeFilter {
            op: TimeOp::Newer,
            secs: 100,
        }
        .to_range(1_000_000);
        assert_eq!(lo, 999_900);
        assert_eq!(hi, i64::MAX);
    }

    #[test]
    fn time_to_range_exact() {
        let (lo, hi) = TimeFilter {
            op: TimeOp::Exact,
            secs: 100,
        }
        .to_range(1_000_000);
        assert_eq!(lo, 999_899);
        assert_eq!(hi, 999_901);
    }

    // ---- parse_size ----

    #[test]
    fn size_larger_mibyte() {
        let r = parse_size("+100M").unwrap();
        assert_eq!(r.op, SizeOp::Larger);
        assert_eq!(r.bytes, 100 << 20);
    }

    #[test]
    fn size_smaller_lower_k() {
        let r = parse_size("-500k").unwrap();
        assert_eq!(r.op, SizeOp::Smaller);
        assert_eq!(r.bytes, 500 << 10);
    }

    #[test]
    fn size_exact_byte_c() {
        let r = parse_size("1024c").unwrap();
        assert_eq!(r.op, SizeOp::Exact);
        assert_eq!(r.bytes, 1024);
    }

    #[test]
    fn size_fractional_gibyte() {
        let r = parse_size("1.5G").unwrap();
        assert_eq!(r.op, SizeOp::Exact);
        assert_eq!(r.bytes, (1.5 * ((1u64 << 30) as f64)) as u64);
    }

    #[test]
    fn size_bare_bytes() {
        let r = parse_size("4096").unwrap();
        assert_eq!(r.op, SizeOp::Exact);
        assert_eq!(r.bytes, 4096);
    }

    #[test]
    fn size_tebi() {
        assert_eq!(parse_size("2T").unwrap().bytes, 2u64 << 40);
    }

    #[test]
    fn size_strips_trailing_ib() {
        assert_eq!(parse_size("10MiB").unwrap().bytes, 10 << 20);
        assert_eq!(parse_size("10mib").unwrap().bytes, 10 << 20);
    }

    #[test]
    fn size_strips_trailing_b_after_unit() {
        assert_eq!(parse_size("500KB").unwrap().bytes, 500 << 10);
        assert_eq!(parse_size("1gb").unwrap().bytes, 1 << 30);
    }

    #[test]
    fn size_rejects_bad_unit() {
        assert!(parse_size("1Z").is_err());
    }

    #[test]
    fn size_rejects_negative_number() {
        assert!(parse_size("+-5M").is_err());
    }

    #[test]
    fn size_rejects_empty() {
        assert!(parse_size("").is_err());
    }

    #[test]
    fn size_rejects_multibyte_tail() {
        assert!(parse_size("1é").is_err());
    }

    #[test]
    fn size_to_bounds_larger() {
        let (lo, hi) = SizeFilter {
            op: SizeOp::Larger,
            bytes: 1024,
        }
        .to_bounds();
        assert_eq!(lo, Some(1024));
        assert_eq!(hi, None);
    }

    #[test]
    fn size_to_bounds_smaller() {
        let (lo, hi) = SizeFilter {
            op: SizeOp::Smaller,
            bytes: 1024,
        }
        .to_bounds();
        assert_eq!(lo, None);
        assert_eq!(hi, Some(1024));
    }

    #[test]
    fn size_to_bounds_exact() {
        let (lo, hi) = SizeFilter {
            op: SizeOp::Exact,
            bytes: 4096,
        }
        .to_bounds();
        assert_eq!(lo, Some(4096));
        assert_eq!(hi, Some(4096));
    }
}
