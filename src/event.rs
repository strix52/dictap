//! The cross-thread vocabulary. Everything the core reacts to arrives as an `Event`.

use crate::gemini::GeminiError;

pub enum Event {
    /// Hotkey or tray: start or stop dictation.
    Toggle,
    Power(PowerEvent),
    Ui(UiCmd),
    Capture {
        sid: u64,
        ev: CaptureEvent,
    },
    Live {
        sid: u64,
        ev: LiveEvent,
    },
    /// The transcript so far while Live is streaming: settled text and the interim tail.
    LiveText {
        sid: u64,
        finals: String,
        interim: String,
    },
    Pasted(crate::paste::Pasted),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    Suspend,
    Resume,
    Lock,
    Unlock,
}

pub enum UiCmd {
    Copy(i64),
    Retry(i64),
    Delete(i64),
    /// Delete every dictation created before this time (unix ms).
    ClearBefore(i64),
    SaveSettings(crate::settings::Settings),
    SetDictionary(Vec<String>),
    SetApiKey(String),
    TestKey,
    ImportOpenWhispr,
    SetAutostart(bool),
}

pub enum CaptureEvent {
    Opened,
    Failed(String),
    /// Capture stopped and the WAV is finalized. `reason` is set when it ended on its own
    /// (device unplugged, stream error).
    Ended {
        duration_ms: u64,
        dropped: u32,
        reason: Option<String>,
    },
}

pub enum LiveEvent {
    /// Final result of this dictation (Live, or batch fallback).
    Done {
        text: String,
        provisional: bool,
        model: &'static str,
        error: Option<GeminiError>,
    },
    /// Live failed before a result. `partial` is whatever it had transcribed; core decides
    /// on the batch fallback once capture has finalized the WAV.
    Failed { error: GeminiError, partial: String },
}
