//! Batch transcription of a finished WAV: the fallback when Live fails, retry from
//! history, and the key test.

use super::GeminiError;
use super::protocol;
use crate::event::{Event, LiveEvent};
use crate::key::Secret;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const URL: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
/// ~20 min of 16 kHz mono. The base64 body is ~4/3 of this, well under the 100 MB
/// inline request limit; Live's 10-minute cap keeps normal dictations far below it.
const MAX_WAV: usize = 40_000_000;
/// Base request timeout, plus one second per `UPLINK` bytes of body (a slow ~1 Mbit/s uplink).
const TIMEOUT: Duration = Duration::from_secs(60);
const UPLINK: usize = 125_000;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls)
            .build()
            .new_agent()
    })
}

fn net_error(e: ureq::Error, key: &str) -> GeminiError {
    match e {
        ureq::Error::Io(_)
        | ureq::Error::Timeout(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Tls(_)
        | ureq::Error::NativeTls(_) => GeminiError::Offline,
        e => GeminiError::Other(super::scrub(&e.to_string(), key)),
    }
}

pub fn transcribe(
    key: &str,
    wav: &[u8],
    language: Option<&str>,
    words: &[String],
) -> Result<String, GeminiError> {
    if key.is_empty() {
        return Err(GeminiError::KeyMissing);
    }
    if wav.len() > MAX_WAV {
        return Err(GeminiError::Other(
            "Recording too long for the fallback".into(),
        ));
    }
    let body = protocol::batch_body(wav, language, words);
    let timeout = TIMEOUT + Duration::from_secs((body.len() / UPLINK) as u64);
    let mut resp = agent()
        .post(URL)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .header("x-goog-api-key", key)
        .header("content-type", "application/json")
        .send(body.as_bytes())
        .map_err(|e| net_error(e, key))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| net_error(e, key))?;
    if status != 200 {
        return Err(GeminiError::from_http(status, &text, key));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| GeminiError::Other("Unreadable response from Gemini".into()))?;
    protocol::batch_text(&v).map_err(|e| GeminiError::Other(super::scrub(&e, key)))
}

/// Checks the key with one second of silence. Anything that isn't a key, rate or network
/// problem counts as working; empty text is fine.
pub fn test_key(key: &str) -> Result<(), GeminiError> {
    let samples = [0i16; 16_000];
    let mut wav = crate::audio::header(samples.len() as u32 * 2).to_vec();
    wav.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    match transcribe(key, &wav, None, &[]) {
        Ok(_) | Err(GeminiError::Other(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

pub struct Job {
    pub key: Arc<Secret>,
    pub language: Option<String>,
    pub words: Vec<String>,
    pub wav: PathBuf,
    /// Live's text so far, kept as provisional if batch fails.
    pub partial: String,
}

/// Runs batch on its own thread and reports the final verdict as `LiveEvent::Done`.
pub fn spawn(sid: u64, job: Job, events: Sender<Event>) {
    std::thread::Builder::new()
        .name("batch".into())
        .spawn(move || {
            let key = job.key.as_str().unwrap_or("");
            let result = crate::audio::read_wav(&job.wav)
                .map_err(|e| GeminiError::Other(format!("Couldn't read audio: {e}")))
                .and_then(|wav| transcribe(key, &wav, job.language.as_deref(), &job.words));
            let ev = match result {
                Ok(text) => {
                    log::info!("batch {sid}: {} chars", text.len());
                    LiveEvent::Done {
                        text,
                        provisional: false,
                        model: protocol::BATCH_MODEL,
                        error: None,
                    }
                }
                Err(e) => {
                    log::warn!("batch {sid}: {e:?}");
                    let provisional = !job.partial.is_empty();
                    LiveEvent::Done {
                        text: job.partial,
                        provisional,
                        model: protocol::LIVE_MODEL,
                        error: Some(e),
                    }
                }
            };
            let _ = events.send(Event::Live { sid, ev });
        })
        .expect("spawn batch thread");
}
