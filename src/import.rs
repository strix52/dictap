//! One-time, read-only import from OpenWhispr: history, dictionary and API key.

/// Parses OpenWhispr timestamps to unix ms:
/// `YYYY-MM-DD[ T]HH:MM:SS[.fff][Z|±HH:MM]`. No zone means UTC (SQLite CURRENT_TIMESTAMP).
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b' ' | b'T') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let digits = format!("{:0<3}", &frac[..n.min(3)]);
        ms = digits.parse::<i64>().ok()?;
        rest = &frac[n..];
    }
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hm = rest[1..].replace(':', "");
            if hm.len() != 4 || !hm.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            sign * (hm[..2].parse::<i64>().ok()? * 60 + hm[2..].parse::<i64>().ok()?)
        }
    };

    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    Some(secs * 1000 + ms)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formats() {
        let base = 1_758_888_000_000; // 2025-09-26T12:00:00Z
        assert_eq!(parse_ts("2025-09-26 12:00:00"), Some(base));
        assert_eq!(parse_ts("2025-09-26T12:00:00Z"), Some(base));
        assert_eq!(parse_ts("2025-09-26 12:00:00.123Z"), Some(base + 123));
        assert_eq!(parse_ts("2025-09-26T12:00:00.5"), Some(base + 500));
        assert_eq!(parse_ts("2025-09-26T12:00:00.123456Z"), Some(base + 123));
        assert_eq!(parse_ts("2025-09-26T13:00:00+01:00"), Some(base));
        assert_eq!(parse_ts("2025-09-26T06:30:00-0530"), Some(base));
        assert_eq!(parse_ts("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_ts("2024-02-29 00:00:00"), Some(1_709_164_800_000));
        for bad in ["", "2025-09-26", "2025/09/26 12:00:00", "2025-13-01 00:00:00", "2025-09-26 12:00:00 junk", "2025-09-26 12:00:00."] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }
}
