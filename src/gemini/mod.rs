pub mod batch;
pub mod live;
pub mod protocol;

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum GeminiError {
    KeyMissing,
    KeyInvalid,
    RateLimited,
    Offline,
    /// Scrubbed, at most ~200 chars.
    Other(String),
}

impl GeminiError {
    /// Maps a batch HTTP error response.
    pub fn from_http(status: u16, body: &str, key: &str) -> GeminiError {
        match status {
            400 if body.contains("API_KEY_INVALID") => GeminiError::KeyInvalid,
            401 | 403 => GeminiError::KeyInvalid,
            429 => GeminiError::RateLimited,
            _ => GeminiError::Other(format!("HTTP {status}: {}", scrub(body, key))),
        }
    }

    /// Maps a Live close code. 1007 = malformed key, 1008 = unknown/revoked key.
    pub fn from_close(code: u16, reason: &str, key: &str) -> GeminiError {
        match code {
            1007 | 1008 => GeminiError::KeyInvalid,
            _ => GeminiError::Other(format!("Live closed ({code}) {}", scrub(reason, key))),
        }
    }

    /// Whether trying the other path (batch) could help.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, GeminiError::KeyMissing | GeminiError::KeyInvalid)
    }
}

impl fmt::Display for GeminiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeminiError::KeyMissing => f.write_str("No Gemini API key. Add one in Settings."),
            GeminiError::KeyInvalid => {
                f.write_str("Gemini API key is invalid. Check it in Settings.")
            }
            GeminiError::RateLimited => {
                f.write_str("Gemini rate limit reached. Try again shortly.")
            }
            GeminiError::Offline => f.write_str("Can't reach Gemini. Check your connection."),
            GeminiError::Other(s) => write!(f, "Gemini error: {s}"),
        }
    }
}

/// Removes the key and any `key=` query value, collapses whitespace, caps length.
pub fn scrub(s: &str, key: &str) -> String {
    let mut out = if key.is_empty() {
        s.to_string()
    } else {
        s.replace(key, "***")
    };
    let mut from = 0;
    while let Some(i) = out[from..].find("key=").map(|i| i + from + 4) {
        let end = out[i..]
            .find(|c: char| c == '&' || c == '"' || c.is_whitespace())
            .map_or(out.len(), |e| i + e);
        out.replace_range(i..end, "***");
        from = i + 3;
    }
    let out: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    match out.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &out[..i]),
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_mapping() {
        assert_eq!(
            GeminiError::from_http(400, r#"{"reason":"API_KEY_INVALID"}"#, "k"),
            GeminiError::KeyInvalid
        );
        assert_eq!(
            GeminiError::from_http(403, "", "k"),
            GeminiError::KeyInvalid
        );
        assert_eq!(
            GeminiError::from_http(429, "", "k"),
            GeminiError::RateLimited
        );
        assert_eq!(
            GeminiError::from_http(500, "boom", "k"),
            GeminiError::Other("HTTP 500: boom".into())
        );
        assert_eq!(
            GeminiError::from_close(1008, "", "k"),
            GeminiError::KeyInvalid
        );
        assert!(GeminiError::from_close(1011, "x", "k").is_retryable());
    }

    #[test]
    fn scrub_removes_secrets() {
        let s = scrub(
            "bad AIzaSECRET at wss://h/x?key=AIzaSECRET&a=1 and key=other\n end",
            "AIzaSECRET",
        );
        assert!(!s.contains("SECRET") && !s.contains("other"), "{s}");
        assert_eq!(s, "bad *** at wss://h/x?key=***&a=1 and key=*** end");
        assert_eq!(scrub(&"x".repeat(300), "").chars().count(), 201);
    }
}
