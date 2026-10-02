//! User settings in `%APPDATA%\dictap\settings.json`. The dictionary lives in the DB,
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
    /// Dictations older than this many days are deleted; 0 keeps everything.
    pub keep_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: Chord::DEFAULT.to_string(),
            language: String::new(),
            sounds: true,
            keep_days: 0,
        }
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

    /// Writes via a temp file that is flushed to disk before it replaces the old one, so a
    /// crash or a full disk leaves either the old settings or the new, never half a file.
    /// The temp file is removed if any step fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        };
        write().inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
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
        let dir = std::env::temp_dir().join(format!("dictap-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());

        let s = Settings {
            hotkey: "Ctrl+Shift+Space".into(),
            language: String::new(),
            sounds: false,
            keep_days: 30,
        };
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

    #[test]
    fn failed_save_reports_the_error_and_leaves_the_old_file_and_no_temp() {
        let dir = std::env::temp_dir().join(format!("dictap-settings-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let old = Settings {
            sounds: false,
            ..Settings::default()
        };
        old.save(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        // The destination is a directory, so the final replace fails.
        let blocked = dir.join("blocked.json");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(Settings::default().save(&blocked).is_err());
        assert!(
            !blocked.with_extension("json.tmp").exists(),
            "temp file cleaned up"
        );

        // A missing parent folder fails up front.
        assert!(
            Settings::default()
                .save(&dir.join("nope").join("s.json"))
                .is_err()
        );
        assert_eq!(Settings::load(&path), old);
        std::fs::remove_dir_all(&dir).ok();
    }
}
