//! The Gemini API key: held as a `Secret`, stored only in Credential Manager.

use crate::win::cred;

const TARGET: &str = "gemdict/gemini-api-key";

/// Bytes zeroed on drop. Never Debug/Display.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Secret {
        Secret(bytes)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// The key as text, trimmed. None if it isn't UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok().map(str::trim)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: valid, aligned pointer into our own buffer; volatile so it isn't elided.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

pub fn load() -> Option<Secret> {
    match cred::read(TARGET) {
        Ok(Some(b)) => Some(Secret::new(b)).filter(|s| s.as_str().is_some_and(|k| !k.is_empty())),
        Ok(None) => None,
        Err(e) => {
            log::warn!("key: credential read failed: {e}");
            None
        }
    }
}

pub fn store(key: &str) -> windows::core::Result<()> {
    cred::write(TARGET, "gemdict", key.trim().as_bytes())
}
