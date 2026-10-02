//! What a transcription attempt produced, independent of how it was obtained.
//!
//! Providers return these; `core::decide_commit` is the only place that turns one (plus the
//! capture's terminal state) into a status, a stored model, and an audio-retention rule.

use crate::gemini::GeminiError;
use std::fmt;

/// Non-empty, outer-trimmed text. Only constructible from text that has content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonEmptyText(String);

impl NonEmptyText {
    pub fn new(s: &str) -> Option<NonEmptyText> {
        let t = s.trim();
        (!t.is_empty()).then(|| NonEmptyText(t.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// A recognized, completed batch response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchText {
    Text(NonEmptyText),
    /// The provider said it finished and heard nothing.
    Empty,
}

/// Why a result is not a clean, confirmed transcript. Messages are short and scrubbed.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    Provider(GeminiError),
    /// The response was not a recognized completed shape.
    Protocol(String),
    /// The user cancelled.
    Cancelled,
    /// Lock, sleep or similar: the machine stepped away from the user.
    Interrupted,
    /// A wait expired (provider or capture).
    Timeout,
    /// The microphone/spool side failed or dropped audio.
    Capture(String),
    /// Live ended (or was stopped) without the provider confirming the whole stream.
    Unconfirmed,
    /// The transcript grew past the aggregate cap.
    TooLarge,
    /// Our own bookkeeping failed (a worker could not start, a queue was gone).
    Internal(String),
}

impl Failure {
    /// Whether this should raise the "may be cut off" warning when text is pasted. An
    /// unconfirmed Live stream is expected on every dictation, so it is recorded (provisional
    /// row, audio kept) without a warning; anything that actually went wrong warns.
    pub fn warns(&self) -> bool {
        !matches!(self, Failure::Unconfirmed)
    }
}

impl Failure {
    /// Whether the batch endpoint is worth trying on the saved audio after Live ended this
    /// way. An unconfirmed Live stream is not an error, but an empty one is re-checked in
    /// batch so silence resolves to a recognized "nothing heard" instead of a failure; the
    /// core handles that case separately from this predicate.
    pub fn wants_fallback(&self) -> bool {
        match self {
            Failure::Provider(e) => e.is_retryable(),
            Failure::Protocol(_) | Failure::Timeout | Failure::Internal(_) | Failure::TooLarge => {
                true
            }
            Failure::Cancelled
            | Failure::Interrupted
            | Failure::Capture(_)
            | Failure::Unconfirmed => false,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Provider(e) => write!(f, "{e}"),
            Failure::Protocol(s) => write!(f, "Unexpected Gemini response: {s}"),
            Failure::Cancelled => f.write_str("Cancelled"),
            Failure::Interrupted => f.write_str("Interrupted (locked or asleep)"),
            Failure::Timeout => f.write_str("Timed out"),
            Failure::Capture(s) => write!(f, "Recording problem: {s}"),
            Failure::Unconfirmed => f.write_str("Live transcript not confirmed complete"),
            Failure::TooLarge => f.write_str("Transcript too long"),
            Failure::Internal(s) => write!(f, "Internal error: {s}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptOutcome {
    /// Confirmed complete, non-empty.
    Complete {
        text: NonEmptyText,
        model: &'static str,
    },
    /// Recognized, completed, and empty.
    Empty {
        model: &'static str,
    },
    /// Text we have but cannot call complete (Live without confirmation, or a failure part-way).
    Incomplete {
        text: String,
        model: &'static str,
        reason: Failure,
    },
    Failed {
        reason: Failure,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_text_trims_and_rejects_blank() {
        assert!(NonEmptyText::new("  \n ").is_none());
        assert_eq!(
            NonEmptyText::new(" hi there \n").unwrap().as_str(),
            "hi there"
        );
    }
}
