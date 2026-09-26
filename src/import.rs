//! One-time, read-only import from OpenWhispr: history, dictionary and API key.
//! Nothing of OpenWhispr's is ever written: its DB is opened read-only, its credential and
//! key files are only read.

use crate::key::Secret;
use crate::store::{self, NewRow, Store};
use crate::win::{aesgcm, cred};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MASTER_TARGET: &str = "secrets-master-key.OpenWhispr";

/// `%APPDATA%\open-whispr`, if it exists.
pub fn openwhispr_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("open-whispr");
    dir.is_dir().then_some(dir)
}

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub imported: usize,
    /// Already imported earlier.
    pub existing: usize,
    /// Rows whose timestamp couldn't be read.
    pub skipped: usize,
    pub words: usize,
}

/// Imports history and dictionary. Safe to repeat: rows are keyed by OpenWhispr's id.
pub fn history(store: &mut Store, dir: &Path) -> Result<Report, String> {
    let src = Connection::open_with_flags(
        dir.join("transcriptions.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Couldn't open OpenWhispr's history: {e}"))?;
    src.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let err = |e: rusqlite::Error| format!("Reading OpenWhispr's history: {e}");

    let rows = read_rows(&src).map_err(err)?;
    let words = read_words(&src).map_err(err)?;
    drop(src);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    store
        .in_tx(|st| {
            let mut report = Report::default();
            for r in &rows {
                let Some(created_ms) = r.timestamp.as_deref().and_then(parse_ts) else {
                    report.skipped += 1;
                    continue;
                };
                let completed = r.status.as_deref().is_none_or(|s| s == "completed");
                let new = NewRow {
                    created_ms,
                    text: r.text.as_deref().unwrap_or(""),
                    duration_ms: r.duration_ms,
                    model: r.model.as_deref(),
                    status: if completed { store::OK } else { store::FAILED },
                    error: r.error.as_deref(),
                    audio_path: None,
                };
                if st.insert_imported("openwhispr", r.id, &new)? {
                    report.imported += 1;
                } else {
                    report.existing += 1;
                }
            }
            report.words = st.add_words(&words)?;
            st.set_meta("openwhispr_import_ms", &now.to_string())?;
            Ok(report)
        })
        .map_err(|e| format!("Saving imported history: {e}"))
}

struct SrcRow {
    id: i64,
    text: Option<String>,
    timestamp: Option<String>,
    duration_ms: Option<i64>,
    model: Option<String>,
    status: Option<String>,
    error: Option<String>,
}

fn columns(c: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = c.prepare("SELECT name FROM pragma_table_info(?1)")?;
    st.query_map([table], |r| r.get(0))?.collect()
}

/// The quoted column if it exists, else `NULL`, so one query works across OpenWhispr versions.
fn col(cols: &[String], name: &str) -> String {
    if cols.iter().any(|c| c == name) {
        format!("\"{name}\"")
    } else {
        "NULL".into()
    }
}

fn deleted_filter(cols: &[String]) -> &'static str {
    if cols.iter().any(|c| c == "deleted_at") {
        " WHERE deleted_at IS NULL"
    } else {
        ""
    }
}

fn read_rows(c: &Connection) -> rusqlite::Result<Vec<SrcRow>> {
    let cols = columns(c, "transcriptions")?;
    let sql = format!(
        "SELECT id, {}, {}, {}, {}, {}, {} FROM transcriptions{} ORDER BY id",
        col(&cols, "text"),
        col(&cols, "timestamp"),
        col(&cols, "audio_duration_ms"),
        col(&cols, "model"),
        col(&cols, "status"),
        col(&cols, "error_message"),
        deleted_filter(&cols),
    );
    let mut st = c.prepare(&sql)?;
    st.query_map([], |r| {
        Ok(SrcRow {
            id: r.get(0)?,
            text: r.get(1)?,
            timestamp: r.get(2)?,
            // Durations may be stored as REAL; read as float and round.
            duration_ms: r.get::<_, Option<f64>>(3)?.map(|d| d.round() as i64),
            model: r.get(4)?,
            status: r.get(5)?,
            error: r.get(6)?,
        })
    })?
    .collect()
}

fn read_words(c: &Connection) -> rusqlite::Result<Vec<String>> {
    let cols = columns(c, "custom_dictionary")?;
    if !cols.iter().any(|c| c == "word") {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT word FROM custom_dictionary{} ORDER BY rowid",
        deleted_filter(&cols)
    );
    let mut st = c.prepare(&sql)?;
    st.query_map([], |r| r.get::<_, Option<String>>(0))?
        .filter_map(Result::transpose)
        .collect()
}

/// Recovers the Gemini key OpenWhispr stored (see its `secretCrypto.js`), falling back to
/// its `.env`. Error text never contains key material.
pub fn api_key(dir: &Path) -> Result<Secret, String> {
    decrypt_key(dir).or_else(|e| {
        log::info!("import: encrypted key unavailable ({e}); trying .env");
        env_key(&dir.join(".env")).ok_or(e)
    })
}

fn decrypt_key(dir: &Path) -> Result<Secret, String> {
    let blob = cred::read(MASTER_TARGET)
        .map_err(|e| format!("Credential Manager: {e}"))?
        .map(Secret::new)
        .ok_or("OpenWhispr's master key isn't in Credential Manager")?;
    // keyring stores text; on Windows that's usually UTF-16LE.
    let b = blob.bytes();
    let utf16 = b.len() >= 2 && b.len() % 2 == 0 && b.iter().skip(1).step_by(2).all(|&x| x == 0);
    let text = Secret::new(if utf16 {
        b.iter().step_by(2).copied().collect()
    } else {
        b.to_vec()
    });
    let master = Secret::new(
        STANDARD
            .decode(text.bytes().trim_ascii())
            .map_err(|_| "OpenWhispr's master key isn't valid base64")?,
    );
    let master: &[u8; 32] = master
        .bytes()
        .try_into()
        .map_err(|_| "OpenWhispr's master key has the wrong length")?;

    let file = std::fs::read(dir.join("secure-keys").join("GEMINI_API_KEY.enc"))
        .map_err(|e| format!("OpenWhispr's key file: {e}"))?;
    if file.len() <= 28 {
        return Err("OpenWhispr's key file is too short".into());
    }
    let (iv, rest) = file.split_at(12);
    let (tag, ct) = rest.split_at(16);
    let plain = Secret::new(aesgcm::decrypt(
        master,
        iv.try_into().expect("12 bytes"),
        tag.try_into().expect("16 bytes"),
        ct,
    )?);
    match plain.as_str() {
        Some(k) if !k.is_empty() => Ok(Secret::new(k.as_bytes().to_vec())),
        _ => Err("OpenWhispr's stored key is empty".into()),
    }
}

fn env_key(path: &Path) -> Option<Secret> {
    let text = Secret::new(std::fs::read(path).ok()?);
    let key = text.as_str()?.lines().find_map(|l| {
        let v = l
            .trim()
            .strip_prefix("GEMINI_API_KEY")?
            .trim_start()
            .strip_prefix('=')?;
        let v = v.trim().trim_matches(['"', '\'']);
        (!v.is_empty()).then(|| v.as_bytes().to_vec())
    })?;
    Some(Secret::new(key))
}

/// Parses OpenWhispr timestamps to unix ms:
/// `YYYY-MM-DD[ T]HH:MM:SS[.fff][Z|±HH:MM]`. No zone means UTC (SQLite CURRENT_TIMESTAMP).
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b' ' | b'T')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
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
        for bad in [
            "",
            "2025-09-26",
            "2025/09/26 12:00:00",
            "2025-13-01 00:00:00",
            "2025-09-26 12:00:00 junk",
            "2025-09-26 12:00:00.",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }
}
