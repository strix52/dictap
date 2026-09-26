//! User settings in `%APPDATA%\gemdict\settings.json`. The dictionary lives in the DB,
//! the API key in Credential Manager.

use crate::hotkey::Chord;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Chord text, e.g. "Ctrl+Win".
    pub hotkey: String,
    /// BCP-47 code; empty means auto-detect (no `languageCodes` sent).
    pub language: String,
    pub sounds: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { hotkey: Chord::DEFAULT.to_string(), language: "en-GB".into(), sounds: true }
    }
}

impl Settings {
    /// Loads settings. A missing file gives defaults; a corrupt one is renamed to
    /// `settings.json.bad` and defaults are used.
    pub fn load(path: &Path) -> Settings {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Settings::default();
        };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("settings.json unreadable ({e}); using defaults");
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Settings::default()
            }
        }
    }

    /// Writes via a temp file and rename so a crash can't leave half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).expect("settings serialize"))?;
        std::fs::rename(tmp, path)
    }

    pub fn chord(&self) -> Chord {
        Chord::parse(&self.hotkey).unwrap_or(Chord::DEFAULT)
    }

    pub fn language(&self) -> Option<&str> {
        Some(self.language.trim()).filter(|l| !l.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_partial_and_corrupt() {
        let dir = std::env::temp_dir().join(format!("gemdict-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());

        let s = Settings { hotkey: "Ctrl+Shift+Space".into(), language: String::new(), sounds: false };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        assert_eq!(Settings::load(&path).language(), None);

        std::fs::write(&path, r#"{"language":"fr-FR"}"#).unwrap();
        let s = Settings::load(&path);
        assert_eq!((s.language(), s.chord()), (Some("fr-FR"), Chord::DEFAULT));

        std::fs::write(&path, "{nope").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(path.with_extension("json.bad").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
