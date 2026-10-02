//! All Win32 `unsafe` lives under this module.

pub mod aesgcm;
pub mod app;
pub mod clipboard;
pub mod cred;
pub mod hook;
pub mod input;
pub mod ipc;
pub mod overlay;
pub mod tray;
pub mod ui;
pub mod window;

/// What a `GetMessageW` return value means. The API returns -1 on error, 0 for WM_QUIT and
/// a positive value for a message; treating -1 as "true" would dispatch a stale `MSG`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pump {
    Error,
    Quit,
    Message,
}

pub fn classify_get_message(result: i32) -> Pump {
    match result {
        -1 => Pump::Error,
        0 => Pump::Quit,
        _ => Pump::Message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_message_results_are_classified() {
        assert_eq!(classify_get_message(-1), Pump::Error);
        assert_eq!(classify_get_message(0), Pump::Quit);
        assert_eq!(classify_get_message(1), Pump::Message);
    }
}
