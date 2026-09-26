//! Gemini Live and batch JSON: building requests and parsing Live server messages.
//! No I/O here, so it's all unit-tested with fixtures.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

pub const LIVE_MODEL: &str = "gemini-3.5-transcribe-live";
pub const BATCH_MODEL: &str = "gemini-3.5-transcribe";
pub const MAX_VOCAB: usize = 100;

/// Frames at or above this RMS count as speech: sending one reopens a turn the server
/// may already have closed on silence (OpenWhispr's measured threshold).
pub const TURN_REOPEN_RMS: f32 = 0.012;

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

/// Text from a batch response, or `Err(status)` when `status` is present and not `completed`.
pub fn batch_text(v: &Value) -> Result<String, String> {
    match v["status"].as_str() {
        None | Some("completed") => {}
        Some(other) => return Err(other.to_string()),
    }
    if let Some(t) = v["output_text"].as_str() {
        return Ok(t.trim().to_string());
    }
    let parts: Vec<&str> = v["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["content"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .collect();
    Ok(parts.join("").trim().to_string())
}

/// Everything a single Live server message can carry. Several fields can be set at once.
#[derive(Default, Debug, PartialEq)]
pub struct ServerMsg {
    pub setup_complete: bool,
    pub interim: Option<String>,
    pub final_text: Option<String>,
    pub generation_complete: bool,
}

pub fn parse_server(text: &str) -> Option<ServerMsg> {
    let v: Value = serde_json::from_str(text).ok()?;
    let sc = &v["serverContent"];
    let text_at = |v: &Value| {
        v["text"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Some(ServerMsg {
        setup_complete: v.get("setupComplete").is_some(),
        interim: text_at(&sc["interimInputTranscription"]),
        final_text: text_at(&sc["inputTranscription"]),
        generation_complete: sc
            .get("generationComplete")
            .is_some_and(|g| g.as_bool() != Some(false)),
    })
}

/// Live transcript state for one dictation.
#[derive(Default)]
pub struct Transcript {
    finals: Vec<String>,
    interim: String,
    /// True while the server may still send a final for audio we sent.
    pub turn_open: bool,
}

impl Transcript {
    pub fn apply(&mut self, m: &ServerMsg) {
        if let Some(i) = &m.interim {
            self.interim = i.clone(); // interims revise the whole partial: replace
            self.turn_open = true;
        }
        if let Some(f) = &m.final_text {
            self.finals.push(f.clone());
            self.interim.clear();
        }
        if m.generation_complete {
            self.turn_open = false;
        }
    }

    /// Call for each audio frame sent; speech reopens the turn.
    pub fn sent_audio(&mut self, samples: &[i16]) {
        if rms(samples) >= TURN_REOPEN_RMS {
            self.turn_open = true;
        }
    }

    /// (settled text, interim tail) for display.
    pub fn parts(&self) -> (String, &str) {
        (self.finals.join(" "), &self.interim)
    }

    /// (text, provisional): finals joined; any trailing interim is added and marks it provisional.
    pub fn result(&self) -> (String, bool) {
        let finals = self.finals.join(" ");
        match (finals.is_empty(), self.interim.is_empty()) {
            (_, true) => (finals, false),
            (true, false) => (self.interim.clone(), true),
            (false, false) => (format!("{finals} {}", self.interim), true),
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

    #[test]
    fn batch_text_variants() {
        assert_eq!(batch_text(&json!({"output_text": " hi "})), Ok("hi".into()));
        assert_eq!(
            batch_text(&json!({"status": "completed", "output_text": "a"})),
            Ok("a".into())
        );
        assert_eq!(
            batch_text(&json!({"status": "failed"})),
            Err("failed".into())
        );
        let steps = json!({"steps": [{"content": [{"text": "Hello "}, {"text": "there"}]}, {"content": []}]});
        assert_eq!(batch_text(&steps), Ok("Hello there".into()));
        assert_eq!(batch_text(&json!({})), Ok(String::new()));
    }

    #[test]
    fn parse_combined_message() {
        let m = parse_server(
            r#"{"serverContent":{"inputTranscription":{"text":" Hello world. "},"generationComplete":true}}"#,
        )
        .unwrap();
        assert_eq!(m.final_text.as_deref(), Some("Hello world."));
        assert!(m.generation_complete);
        assert!(
            parse_server(r#"{"setupComplete":{}}"#)
                .unwrap()
                .setup_complete
        );
        assert_eq!(
            parse_server(r#"{"serverContent":{"speechState":"x"}}"#).unwrap(),
            ServerMsg::default()
        );
        assert!(parse_server("not json").is_none());
    }

    #[test]
    fn transcript_turns() {
        let mut t = Transcript::default();
        let msg = |s: &str| parse_server(s).unwrap();
        t.apply(&msg(
            r#"{"serverContent":{"interimInputTranscription":{"text":"the quick"}}}"#,
        ));
        assert!(t.turn_open);
        assert_eq!(t.result(), ("the quick".into(), true));
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"The quick fox."},"generationComplete":true}}"#,
        ));
        assert!(!t.turn_open);
        assert_eq!(t.result(), ("The quick fox.".into(), false));
        t.sent_audio(&[0; 1600]);
        assert!(!t.turn_open, "silence keeps the turn closed");
        t.sent_audio(&[3000, -3000].repeat(800));
        assert!(t.turn_open, "speech reopens it");
        t.apply(&msg(
            r#"{"serverContent":{"inputTranscription":{"text":"Jumps."}}}"#,
        ));
        t.apply(&msg(r#"{"serverContent":{"generationComplete":true}}"#));
        assert_eq!(t.result(), ("The quick fox. Jumps.".into(), false));
    }
}
