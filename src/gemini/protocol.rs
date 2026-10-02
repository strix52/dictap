//! Gemini Live and batch JSON: building requests and parsing server messages.
//! No I/O here, so it's all unit-tested with fixtures.

use crate::outcome::{BatchText, Failure, NonEmptyText, TranscriptOutcome};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use std::fmt;

pub const LIVE_MODEL: &str = "gemini-3.5-transcribe-live";
pub const BATCH_MODEL: &str = "gemini-3.5-transcribe";
pub const MAX_VOCAB: usize = 100;

/// Aggregate cap on Live transcript text, far above a normal ten-minute dictation.
pub const MAX_TRANSCRIPT_BYTES: usize = 2 << 20;

fn vocab(words: &[String]) -> &[String] {
    &words[..words.len().min(MAX_VOCAB)]
}

pub fn live_setup(language: Option<&str>, words: &[String]) -> String {
    let mut t = json!({});
    if let Some(lang) = language {
        t["languageCodes"] = json!([lang]);
    }
    if !words.is_empty() {
        t["customVocabulary"] = json!(vocab(words));
    }
    json!({"setup": {
        "model": format!("models/{LIVE_MODEL}"),
        "generationConfig": {"responseModalities": ["TEXT"]},
        "inputAudioTranscription": t,
    }})
    .to_string()
}

/// One `realtimeInput` audio message for 16 kHz mono i16 samples.
pub fn live_audio(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    json!({"realtimeInput": {"audio": {"data": B64.encode(bytes), "mimeType": "audio/pcm;rate=16000"}}})
        .to_string()
}

pub const LIVE_END: &str = r#"{"realtimeInput":{"audioStreamEnd":true}}"#;

pub fn batch_body(wav: &[u8], language: Option<&str>, words: &[String]) -> String {
    let mut t = json!({});
    if let Some(lang) = language {
        t["language_codes"] = json!([lang]);
    }
    if !words.is_empty() {
        t["custom_vocabulary"] = json!(vocab(words));
    }
    json!({
        "model": BATCH_MODEL,
        "input": [{"type": "audio", "data": B64.encode(wav), "mime_type": "audio/wav"}],
        "generation_config": {"transcription_config": t},
    })
    .to_string()
}

/// Why a batch response was not a recognized completed interaction.
#[derive(Debug, PartialEq)]
pub enum BatchProtocolError {
    /// The interaction reported a status other than `completed`.
    Status(String),
    /// A required field is missing or has the wrong type.
    Shape(&'static str),
}

impl BatchProtocolError {
    /// A short scrubbed error; never includes the response body.
    pub fn into_error(self, key: &str) -> super::GeminiError {
        let msg = match self {
            BatchProtocolError::Status(s) => format!("interaction {}", super::scrub(&s, key)),
            BatchProtocolError::Shape(what) => format!("unrecognized response ({what})"),
        };
        super::GeminiError::Other(msg)
    }
}

/// Reads a completed Interactions response:
/// `{"status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":".."}]}]}`.
/// Only text content of `model_output` steps counts; other steps and content types are
/// ignored. A completed response with no text is `Empty`. Anything that does not have that
/// envelope (`{}`, a missing status, wrong field types) is an error, never silence.
pub fn parse_batch(v: &Value) -> Result<BatchText, BatchProtocolError> {
    use BatchProtocolError::{Shape, Status};
    let status = v
        .get("status")
        .ok_or(Shape("missing status"))?
        .as_str()
        .ok_or(Shape("status is not a string"))?;
    if status != "completed" {
        return Err(Status(status.to_string()));
    }
    let steps = v
        .get("steps")
        .ok_or(Shape("missing steps"))?
        .as_array()
        .ok_or(Shape("steps is not an array"))?;
    let mut text = String::new();
    for step in steps {
        let step = step.as_object().ok_or(Shape("step is not an object"))?;
        let kind = step
            .get("type")
            .ok_or(Shape("step without type"))?
            .as_str()
            .ok_or(Shape("step type is not a string"))?;
        if kind != "model_output" {
            continue;
        }
        let content = step
            .get("content")
            .ok_or(Shape("model output without content"))?
            .as_array()
            .ok_or(Shape("content is not an array"))?;
        for part in content {
            let part = part
                .as_object()
                .ok_or(Shape("content part is not an object"))?;
            let kind = part
                .get("type")
                .ok_or(Shape("content part without type"))?
                .as_str()
                .ok_or(Shape("content type is not a string"))?;
            if kind != "text" {
                continue;
            }
            text.push_str(
                part.get("text")
                    .ok_or(Shape("text part without text"))?
                    .as_str()
                    .ok_or(Shape("text is not a string"))?,
            );
        }
    }
    Ok(match NonEmptyText::new(&text) {
        Some(t) => BatchText::Text(t),
        None => BatchText::Empty,
    })
}

/// Everything a single Live server message can carry. Several fields can be set at once.
#[derive(Default, Debug, PartialEq)]
pub struct ServerMsg {
    pub setup_complete: bool,
    /// `Some("")` is an explicit empty interim (clears the partial); `None` means absent.
    pub interim: Option<String>,
    /// `Some("")` is an explicit empty final segment; `None` means no final in this message.
    pub final_text: Option<String>,
    pub generation_complete: bool,
}

#[derive(Debug, PartialEq)]
pub enum LiveParseError {
    NotJson,
    WrongType(&'static str),
}

impl fmt::Display for LiveParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveParseError::NotJson => f.write_str("not JSON"),
            LiveParseError::WrongType(what) => write!(f, "wrong type for {what}"),
        }
    }
}

/// `v[key]` when it is an object, `Ok(None)` when absent, an error when it has another type.
fn object_at<'a>(
    v: &'a Value,
    key: &str,
    what: &'static str,
) -> Result<Option<&'a Value>, LiveParseError> {
    match v.get(key) {
        None => Ok(None),
        Some(o) if o.is_object() => Ok(Some(o)),
        Some(_) => Err(LiveParseError::WrongType(what)),
    }
}

fn transcription_text(
    v: Option<&Value>,
    what: &'static str,
) -> Result<Option<String>, LiveParseError> {
    let Some(t) = v.and_then(|o| o.get("text")) else {
        return Ok(None);
    };
    t.as_str()
        .map(|s| Some(s.trim().to_string()))
        .ok_or(LiveParseError::WrongType(what))
}

pub fn parse_server(text: &str) -> Result<ServerMsg, LiveParseError> {
    let v: Value = serde_json::from_str(text).map_err(|_| LiveParseError::NotJson)?;
    let content = object_at(&v, "serverContent", "serverContent")?;
    let mut msg = ServerMsg {
        setup_complete: v.get("setupComplete").is_some(),
        ..ServerMsg::default()
    };
    if let Some(sc) = content {
        msg.interim = transcription_text(
            object_at(sc, "interimInputTranscription", "interimInputTranscription")?,
            "interim text",
        )?;
        msg.final_text = transcription_text(
            object_at(sc, "inputTranscription", "inputTranscription")?,
            "final text",
        )?;
        msg.generation_complete = match sc.get("generationComplete") {
            None => false,
            Some(g) => g
                .as_bool()
                .ok_or(LiveParseError::WrongType("generationComplete"))?,
        };
    }
    Ok(msg)
}

/// The documented rule that would prove a Live transcript covers every uploaded frame.
///
/// There is none: the Live API describes input transcription as delivered independently of
/// other server messages and defines no acknowledgment for the whole stream (see the
/// execution log's provider evidence gate). The type is uninhabited, so nothing can build a
/// `Complete` Live outcome from elapsed time, `generationComplete` or audio energy; adding a
/// rule later means giving this type a variant, which forces every `match` on it to be written.
pub enum CompletionEvidence {}

/// Live transcript state for one dictation.
#[derive(Default)]
pub struct Transcript {
    finals: Vec<String>,
    interim: String,
    /// Bytes held in `finals`, separators included.
    final_bytes: usize,
    empty_finals: u32,
    overflow: bool,
    generation_complete: bool,
}

impl Transcript {
    pub fn apply(&mut self, m: &ServerMsg) {
        if m.generation_complete {
            self.generation_complete = true;
        }
        if self.overflow {
            return;
        }
        if let Some(i) = &m.interim {
            // Interims revise the whole partial: replace.
            match self.final_bytes.checked_add(i.len()) {
                Some(total) if total <= MAX_TRANSCRIPT_BYTES => self.interim.clone_from(i),
                _ => {
                    self.overflow = true;
                    return;
                }
            }
        }
        if let Some(f) = &m.final_text {
            if f.is_empty() {
                self.empty_finals = self.empty_finals.saturating_add(1);
            } else {
                match self.final_bytes.checked_add(f.len() + 1) {
                    Some(total) if total <= MAX_TRANSCRIPT_BYTES => {
                        self.final_bytes = total;
                        self.finals.push(f.clone());
                    }
                    _ => {
                        self.overflow = true;
                        return;
                    }
                }
            }
            self.interim.clear();
        }
    }

    /// Forget `generationComplete` seen so far; call when the end of stream is flushed.
    pub fn mark_end_flushed(&mut self) {
        self.generation_complete = false;
    }

    /// `generationComplete` seen since `mark_end_flushed`. Only a hint that waiting longer is
    /// unlikely to bring more text; it is never completion evidence.
    #[cfg(test)]
    pub fn generation_complete_after_end(&self) -> bool {
        self.generation_complete
    }

    /// (settled text, interim tail) for display.
    pub fn parts(&self) -> (String, &str) {
        (self.finals.join(" "), &self.interim)
    }

    /// Finals joined; a trailing interim is appended.
    pub fn text(&self) -> String {
        let finals = self.finals.join(" ");
        match (finals.is_empty(), self.interim.is_empty()) {
            (_, true) => finals,
            (true, false) => self.interim.clone(),
            (false, false) => format!("{finals} {}", self.interim),
        }
    }

    #[cfg(test)]
    pub fn empty_finals(&self) -> u32 {
        self.empty_finals
    }

    /// The outcome once the stream is over; `end` is how the connection ended. Without
    /// completion evidence (there is none today) text is `Incomplete` and no text is `Failed`:
    /// an empty Live result cannot be told apart from audio that was never transcribed.
    pub fn outcome(&self, end: Result<(), Failure>) -> TranscriptOutcome {
        let evidence: Option<CompletionEvidence> = None;
        if let Some(e) = evidence {
            match e {}
        }
        let reason = if self.overflow {
            Failure::TooLarge
        } else {
            end.err().unwrap_or(Failure::Unconfirmed)
        };
        let text = self.text();
        if text.is_empty() {
            TranscriptOutcome::Failed { reason }
        } else {
            TranscriptOutcome::Incomplete {
                text,
                model: LIVE_MODEL,
                reason,
            }
        }
    }
}

pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| (f64::from(s) / 32768.0).powi(2))
        .sum();
    (sum / samples.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(s: &str) -> ServerMsg {
        parse_server(s).unwrap()
    }

    #[test]
    fn setup_shapes() {
        let v: Value = serde_json::from_str(&live_setup(None, &[])).unwrap();
        assert_eq!(v["setup"]["model"], "models/gemini-3.5-transcribe-live");
        assert_eq!(v["setup"]["inputAudioTranscription"], json!({}));
        let words: Vec<String> = (0..150).map(|i| format!("w{i}")).collect();
        let v: Value = serde_json::from_str(&live_setup(Some("en-GB"), &words)).unwrap();
        let t = &v["setup"]["inputAudioTranscription"];
        assert_eq!(t["languageCodes"], json!(["en-GB"]));
        assert_eq!(t["customVocabulary"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn audio_and_batch_shapes() {
        let v: Value = serde_json::from_str(&live_audio(&[1, -1])).unwrap();
        assert_eq!(v["realtimeInput"]["audio"]["data"], "AQD//w==");
        let v: Value =
            serde_json::from_str(&batch_body(b"RIFF", Some("en-GB"), &["Orca".into()])).unwrap();
        assert_eq!(v["input"][0]["mime_type"], "audio/wav");
        let tc = &v["generation_config"]["transcription_config"];
        assert_eq!(tc["custom_vocabulary"], json!(["Orca"]));
        assert_eq!(tc["language_codes"], json!(["en-GB"]));
    }

    fn text_of(b: BatchText) -> String {
        match b {
            BatchText::Text(t) => t.into_string(),
            BatchText::Empty => String::new(),
        }
    }

    #[test]
    fn batch_accepts_documented_completed_shapes() {
        let one = json!({"status": "completed", "steps": [
            {"type": "model_output", "content": [{"type": "text", "text": " Hello "}, {"type": "text", "text": "there. "}]}
        ]});
        assert_eq!(text_of(parse_batch(&one).unwrap()), "Hello there.");

        // Text parts keep the provider's spacing; unrelated steps and content are ignored.
        let mixed = json!({"status": "completed", "steps": [
            {"type": "user_input", "content": [{"type": "text", "text": "ignored"}]},
            {"type": "thought", "summary": "x"},
            {"type": "model_output", "content": [
                {"type": "text", "text": "a"}, {"type": "image", "data": "x"}, {"type": "text", "text": "b"}]},
            {"type": "model_output", "content": [{"type": "text", "text": " c"}]},
        ], "id": "i1", "usage": {"total_tokens": 3}});
        assert_eq!(text_of(parse_batch(&mixed).unwrap()), "ab c");
    }

    #[test]
    fn batch_recognized_silence_is_empty_not_error() {
        for v in [
            json!({"status": "completed", "steps": []}),
            json!({"status": "completed", "steps": [{"type": "model_output", "content": []}]}),
            json!({"status": "completed", "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": "  \n"}]}]}),
            json!({"status": "completed", "steps": [{"type": "thought"}]}),
        ] {
            assert_eq!(parse_batch(&v), Ok(BatchText::Empty), "{v}");
        }
    }

    #[test]
    fn batch_rejects_everything_else() {
        use BatchProtocolError::*;
        assert_eq!(parse_batch(&json!({})), Err(Shape("missing status")));
        assert_eq!(parse_batch(&json!(null)), Err(Shape("missing status")));
        assert_eq!(parse_batch(&json!([])), Err(Shape("missing status")));
        assert_eq!(
            parse_batch(&json!({"output_text": "hi"})),
            Err(Shape("missing status")),
            "the SDK convenience field is not the raw contract"
        );
        assert_eq!(
            parse_batch(&json!({"status": "completed", "output_text": "hi"})),
            Err(Shape("missing steps"))
        );
        assert_eq!(
            parse_batch(&json!({"status": 3, "steps": []})),
            Err(Shape("status is not a string"))
        );
        for status in [
            "failed",
            "in_progress",
            "cancelled",
            "requires_action",
            "weird",
        ] {
            assert_eq!(
                parse_batch(&json!({"status": status, "steps": []})),
                Err(Status(status.into()))
            );
        }
        let bad = |steps: Value| parse_batch(&json!({"status": "completed", "steps": steps}));
        assert_eq!(bad(json!({})), Err(Shape("steps is not an array")));
        assert_eq!(bad(json!([1])), Err(Shape("step is not an object")));
        assert_eq!(bad(json!([{}])), Err(Shape("step without type")));
        assert_eq!(
            bad(json!([{"type": 1}])),
            Err(Shape("step type is not a string"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output"}])),
            Err(Shape("model output without content"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output", "content": "x"}])),
            Err(Shape("content is not an array"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output", "content": ["x"]}])),
            Err(Shape("content part is not an object"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output", "content": [{"text": "x"}]}])),
            Err(Shape("content part without type"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output", "content": [{"type": "text"}]}])),
            Err(Shape("text part without text"))
        );
        assert_eq!(
            bad(json!([{"type": "model_output", "content": [{"type": "text", "text": 5}]}])),
            Err(Shape("text is not a string"))
        );
    }

    #[test]
    fn batch_errors_are_scrubbed() {
        let e = BatchProtocolError::Status("failed key=SECRET".into()).into_error("SECRET");
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    #[test]
    fn parse_combined_message() {
        let m = msg(
            r#"{"serverContent":{"inputTranscription":{"text":" Hello world. "},"generationComplete":true}}"#,
        );
        assert_eq!(m.final_text.as_deref(), Some("Hello world."));
        assert!(m.generation_complete);
        assert!(msg(r#"{"setupComplete":{}}"#).setup_complete);
        assert_eq!(
            msg(r#"{"serverContent":{"speechState":"x"}}"#),
            ServerMsg::default()
        );
        assert_eq!(parse_server("not json"), Err(LiveParseError::NotJson));
    }

    #[test]
    fn parse_keeps_explicit_empty_distinct_from_absent() {
        let m = msg(r#"{"serverContent":{"inputTranscription":{"text":"  "}}}"#);
        assert_eq!(m.final_text.as_deref(), Some(""));
        let m = msg(r#"{"serverContent":{"inputTranscription":{}}}"#);
        assert_eq!(m.final_text, None);
        let m = msg(r#"{"serverContent":{"interimInputTranscription":{"text":""}}}"#);
        assert_eq!(m.interim.as_deref(), Some(""));
    }

    #[test]
    fn parse_rejects_wrong_types() {
        let wrong = |s: &str| matches!(parse_server(s), Err(LiveParseError::WrongType(_)));
        assert!(wrong(r#"{"serverContent":[]}"#));
        assert!(wrong(r#"{"serverContent":{"inputTranscription":"hi"}}"#));
        assert!(wrong(
            r#"{"serverContent":{"inputTranscription":{"text":7}}}"#
        ));
        assert!(wrong(
            r#"{"serverContent":{"interimInputTranscription":{"text":null}}}"#
        ));
        assert!(wrong(r#"{"serverContent":{"generationComplete":"yes"}}"#));
        // Unrelated valid fields are tolerated and change nothing.
        let m = msg(r#"{"usageMetadata":{"totalTokenCount":3},"serverContent":{"modelTurn":{}}}"#);
        assert_eq!(m, ServerMsg::default());
    }

    #[test]
    fn generation_complete_before_final_does_not_close_anything() {
        // Input transcription is delivered independently of other messages: the final can
        // arrive after generationComplete, and must still be kept.
        let mut t = Transcript::default();
        t.apply(&msg(r#"{"serverContent":{"generationComplete":true}}"#));
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"Late words."}}}"#,
        ));
        assert_eq!(t.text(), "Late words.");
        assert!(t.generation_complete_after_end());
        t.mark_end_flushed();
        assert!(!t.generation_complete_after_end());
    }

    #[test]
    fn finals_interims_and_repeats() {
        let mut t = Transcript::default();
        t.apply(&msg(
            r#"{"serverContent":{"interimInputTranscription":{"text":"the quick"}}}"#,
        ));
        assert_eq!(t.text(), "the quick");
        t.apply(&msg(
            r#"{"serverContent":{"interimInputTranscription":{"text":"the quick brown"}}}"#,
        ));
        assert_eq!(t.parts(), (String::new(), "the quick brown"));
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"The quick brown."}}}"#,
        ));
        assert_eq!(t.parts(), ("The quick brown.".into(), ""));
        let no = msg(r#"{"serverContent":{"inputTranscription":{"text":"no no"}}}"#);
        t.apply(&no);
        t.apply(&no);
        assert_eq!(t.text(), "The quick brown. no no no no", "repeats are kept");
        t.apply(&msg(
            r#"{"serverContent":{"interimInputTranscription":{"text":"tail"}}}"#,
        ));
        assert_eq!(t.text(), "The quick brown. no no no no tail");
    }

    #[test]
    fn explicit_empty_final_is_counted_not_joined() {
        let mut t = Transcript::default();
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"a"}}}"#,
        ));
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":""}}}"#,
        ));
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"b"}}}"#,
        ));
        assert_eq!(t.text(), "a b");
        assert_eq!(t.empty_finals(), 1);
    }

    #[test]
    fn live_never_completes_without_evidence() {
        let hello = msg(
            r#"{"serverContent":{"inputTranscription":{"text":"Hello."},"generationComplete":true}}"#,
        );
        // A clean end with generationComplete after the end flush is still not Complete.
        let mut t = Transcript::default();
        t.apply(&hello);
        t.mark_end_flushed();
        t.apply(&msg(r#"{"serverContent":{"generationComplete":true}}"#));
        assert!(t.generation_complete_after_end());
        assert_eq!(
            t.outcome(Ok(())),
            TranscriptOutcome::Incomplete {
                text: "Hello.".into(),
                model: LIVE_MODEL,
                reason: Failure::Unconfirmed
            }
        );
        // Text, then the connection fails before a confirmed end.
        let mut t = Transcript::default();
        t.apply(&hello);
        assert!(matches!(
            t.outcome(Err(Failure::Timeout)),
            TranscriptOutcome::Incomplete {
                reason: Failure::Timeout,
                ..
            }
        ));
        // Interim only is still only provisional text.
        let mut t = Transcript::default();
        t.apply(&msg(
            r#"{"serverContent":{"interimInputTranscription":{"text":"hel"}}}"#,
        ));
        assert!(matches!(
            t.outcome(Ok(())),
            TranscriptOutcome::Incomplete { .. }
        ));
        // Nothing transcribed (even with an explicit empty final) is a failure, not silence.
        let mut t = Transcript::default();
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":""},"generationComplete":true}}"#,
        ));
        assert_eq!(
            t.outcome(Ok(())),
            TranscriptOutcome::Failed {
                reason: Failure::Unconfirmed
            }
        );
    }

    #[test]
    fn transcript_overflow_is_incomplete_and_bounded() {
        let mut t = Transcript::default();
        let big = "x".repeat(MAX_TRANSCRIPT_BYTES / 2);
        let m = |text: &str| ServerMsg {
            final_text: Some(text.into()),
            ..ServerMsg::default()
        };
        t.apply(&m(&big));
        t.apply(&m(&big));
        t.apply(&m("more")); // would exceed
        assert!(matches!(
            t.outcome(Ok(())),
            TranscriptOutcome::Incomplete {
                reason: Failure::TooLarge,
                ..
            }
        ));
        let held = t.text().len();
        t.apply(&m("later"));
        assert_eq!(t.text().len(), held, "nothing is added after overflow");
        assert!(held <= MAX_TRANSCRIPT_BYTES);
        // An oversize interim also overflows without being kept.
        let mut t = Transcript::default();
        t.apply(&ServerMsg {
            interim: Some("y".repeat(MAX_TRANSCRIPT_BYTES + 1)),
            ..ServerMsg::default()
        });
        assert_eq!(t.text(), "");
        assert!(matches!(
            t.outcome(Ok(())),
            TranscriptOutcome::Failed {
                reason: Failure::TooLarge
            }
        ));
    }

    #[test]
    fn duplicate_terminal_messages_do_not_change_text() {
        let mut t = Transcript::default();
        t.apply(&msg(r#"{"serverContent":{"generationComplete":true}}"#));
        t.apply(&msg(r#"{"serverContent":{"generationComplete":true}}"#));
        assert_eq!(t.text(), "");
    }
}
