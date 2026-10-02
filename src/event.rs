//! The cross-thread vocabulary. Everything the core reacts to arrives as an `Event`.

use crate::capture::CaptureReport;
use crate::gemini::GeminiError;
use crate::outcome::{BatchText, TranscriptOutcome};
use std::sync::atomic::{AtomicU64, Ordering};

/// One dictation, from hotkey press to its stored result. Also its spool file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// One batch request (fallback, Retry or key check). Disjoint from `SessionId` so a Retry
/// result can never be mistaken for the active dictation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobId(pub u64);

/// One UI request or core-originated notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

impl RequestId {
    /// Process-wide unique ids: the UI thread and the core both allocate from this.
    pub fn next() -> RequestId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        RequestId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

pub enum Event {
    /// Hotkey or tray: start or stop dictation.
    Toggle,
    Power(PowerEvent),
    Ui(ActionRequest),
    Capture {
        session: SessionId,
        ev: CaptureEvent,
    },
    /// The transcript so far while Live is streaming: settled text and the interim tail.
    LiveText {
        session: SessionId,
        finals: String,
        interim: String,
    },
    /// The Live worker's terminal result. Always the last thing the worker does.
    Live {
        session: SessionId,
        outcome: TranscriptOutcome,
    },
    /// A batch worker's terminal result.
    Batch {
        job: JobId,
        result: Result<BatchText, GeminiError>,
    },
    Pasted(crate::paste::Pasted),
    /// A history Copy finished: the clipboard took the text, or didn't.
    Copied {
        request: RequestId,
        result: Result<(), ActionFailure>,
    },
    /// A key check's terminal result.
    KeyTest {
        job: JobId,
        result: Result<(), GeminiError>,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    Suspend,
    Resume,
    Lock,
    Unlock,
}

pub enum CaptureEvent {
    /// The microphone is running.
    Opened,
    /// The capture thread is done and reports what it left behind. Always sent exactly once.
    Finished(CaptureReport),
}

/// A request from the UI or tray. The core answers each one with an `ActionResult`.
pub struct ActionRequest {
    pub id: RequestId,
    /// Which History/Settings window the request came from; replies for a closed window
    /// are still drained but cannot touch a different one.
    pub window_generation: u64,
    pub action: Action,
}

pub enum Action {
    Copy(i64),
    CopyLatest,
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

#[derive(Clone, Debug, PartialEq)]
pub struct ActionResult {
    pub id: RequestId,
    pub window_generation: u64,
    pub result: Result<ActionSuccess, ActionFailure>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ActionSuccess {
    Copied,
    Done,
    /// The key is stored. `test_started` says whether a check is running; if not, the key
    /// is saved but unchecked.
    KeySaved {
        test_started: bool,
    },
    KeyChecked,
    SettingsSaved,
    DictionarySaved,
}

/// A short, scrubbed reason. Never carries a transcript or a key.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionFailure(pub String);
