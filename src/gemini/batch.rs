//! Batch transcription of a finished WAV: the fallback when Live fails, retry from
//! history, and the key test.

use super::GeminiError;
use super::protocol;
use crate::event::{Event, JobId};
use crate::key::Secret;
use crate::outcome::BatchText;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const URL: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
/// ~20 min of 16 kHz mono. The base64 body is ~4/3 of this, well under the 100 MB
/// inline request limit; Live's 10-minute cap keeps normal dictations far below it.
const MAX_WAV: usize = 40_000_000;
/// Largest response body read, for success and error responses alike.
pub const MAX_RESPONSE: usize = 2 << 20;
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

/// Why a response body could not be used.
#[derive(Debug, PartialEq)]
pub enum BodyError {
    TooLarge,
    Unreadable,
}

/// Reads at most `max` bytes; one more byte than that is an error, so a body that keeps
/// coming is never buffered past `max + 1`.
pub fn read_capped(reader: impl Read, max: usize) -> Result<Vec<u8>, BodyError> {
    let mut body = Vec::new();
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| BodyError::Unreadable)?;
    if body.len() > max {
        return Err(BodyError::TooLarge);
    }
    Ok(body)
}

/// Turns an HTTP status and (possibly unreadable) body into a batch result. Pure, so the
/// whole mapping is tested without a network.
fn interpret(
    status: u16,
    body: Result<Vec<u8>, BodyError>,
    key: &str,
) -> Result<BatchText, GeminiError> {
    if status != 200 {
        // The status is kept even when the body can't be used.
        let text = match body {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(BodyError::TooLarge) => "(response too large)".into(),
            Err(BodyError::Unreadable) => "(response unreadable)".into(),
        };
        return Err(GeminiError::from_http(status, &text, key));
    }
    let body = body.map_err(|e| match e {
        BodyError::TooLarge => GeminiError::Other("Response from Gemini too large".into()),
        BodyError::Unreadable => GeminiError::Offline,
    })?;
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| GeminiError::Other("Unreadable response from Gemini".into()))?;
    protocol::parse_batch(&v).map_err(|e| e.into_error(key))
}

fn cancelled() -> GeminiError {
    GeminiError::Other("Cancelled".into())
}

pub fn transcribe(
    key: &str,
    wav: &[u8],
    language: Option<&str>,
    words: &[String],
    cancel: &AtomicBool,
) -> Result<BatchText, GeminiError> {
    if key.is_empty() {
        return Err(GeminiError::KeyMissing);
    }
    if wav.len() > MAX_WAV {
        return Err(GeminiError::Other(
            "Recording too long for the fallback".into(),
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let body = protocol::batch_body(wav, language, words);
    let timeout = timeout(body.len());
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
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
    let body = read_capped(resp.body_mut().as_reader(), MAX_RESPONSE);
    interpret(status, body, key)
}

/// One second of silence as a finished WAV.
fn silence_wav() -> Vec<u8> {
    let samples = [0i16; 16_000];
    let mut wav = crate::audio::header(samples.len() as u32 * 2).to_vec();
    wav.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    wav
}

/// Checks the key with one second of silence. Only a recognized completed response (text or
/// silence) shows the key works; HTTP errors, odd bodies and oversize responses do not.
pub fn test_key(key: &str, cancel: &AtomicBool) -> Result<(), GeminiError> {
    transcribe(key, &silence_wav(), None, &[], cancel).map(|_: BatchText| ())
}

pub struct Job {
    pub key: Arc<Secret>,
    pub language: Option<String>,
    pub words: Vec<String>,
    pub wav: PathBuf,
    pub cancel: Arc<AtomicBool>,
}

fn timeout(body_len: usize) -> Duration {
    TIMEOUT + Duration::from_secs((body_len / UPLINK) as u64)
}

/// The longest a batch run of this WAV can take (base64 grows the body by a third).
pub fn max_wait(wav_len: u64) -> Duration {
    timeout(wav_len as usize / 3 * 4 + 64 * 1024)
}

fn run(job: &Job) -> Result<BatchText, GeminiError> {
    if job.cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let key = job.key.as_str().unwrap_or("");
    let wav = crate::audio::read_wav_bounded(&job.wav, MAX_WAV)
        .map_err(|e| GeminiError::Other(format!("Couldn't read audio: {e}")))?;
    transcribe(key, &wav, job.language.as_deref(), &job.words, &job.cancel)
}

/// Runs batch on its own thread and reports the verdict as `Event::Batch`. The thread always
/// reports once it exists. An `Err` means no thread was started, so nothing will report.
pub fn spawn(id: JobId, job: Job, events: Sender<Event>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("batch".into())
        .spawn(move || {
            let result = run(&job);
            match &result {
                Ok(BatchText::Text(t)) => log::info!("batch {}: {} chars", id.0, t.as_str().len()),
                Ok(BatchText::Empty) => log::info!("batch {}: no speech", id.0),
                Err(e) => log::warn!("batch {}: {e:?}", id.0),
            }
            let _ = events.send(Event::Batch { job: id, result });
        })
        .map(|_| ())
}

/// Same, for the key check. `cancel` is set when the check is no longer wanted.
pub fn spawn_key_test(
    id: JobId,
    key: Arc<Secret>,
    cancel: Arc<AtomicBool>,
    events: Sender<Event>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("key-test".into())
        .spawn(move || {
            let result = test_key(key.as_str().unwrap_or(""), &cancel);
            let _ = events.send(Event::KeyTest { job: id, result });
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"{"status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"Hi there"}]}]}"#;
    const SILENT: &str = r#"{"status":"completed","steps":[]}"#;

    fn ok(s: &str) -> Result<Vec<u8>, BodyError> {
        Ok(s.as_bytes().to_vec())
    }

    #[test]
    fn recognized_responses_are_results() {
        match interpret(200, ok(OK), "k") {
            Ok(BatchText::Text(t)) => assert_eq!(t.as_str(), "Hi there"),
            other => panic!("{other:?}"),
        }
        assert_eq!(interpret(200, ok(SILENT), "k"), Ok(BatchText::Empty));
    }

    #[test]
    fn unrecognized_or_failed_bodies_are_errors() {
        for body in [
            "{}",
            "null",
            "not json",
            "",
            r#"{"output_text":"hi"}"#,
            r#"{"status":"failed","steps":[]}"#,
        ] {
            assert!(
                matches!(interpret(200, ok(body), "k"), Err(GeminiError::Other(_))),
                "{body}"
            );
        }
        assert!(matches!(
            interpret(200, Err(BodyError::TooLarge), "k"),
            Err(GeminiError::Other(m)) if m.contains("too large")
        ));
        assert_eq!(
            interpret(200, Err(BodyError::Unreadable), "k"),
            Err(GeminiError::Offline)
        );
    }

    #[test]
    fn http_errors_keep_their_status_even_without_a_body() {
        let key = |r| matches!(r, Err(GeminiError::KeyInvalid));
        assert!(key(interpret(401, Err(BodyError::TooLarge), "k")));
        assert!(key(interpret(
            400,
            ok(r#"{"error":{"details":[{"reason":"API_KEY_INVALID"}]}}"#),
            "k"
        )));
        assert_eq!(
            interpret(429, Err(BodyError::Unreadable), "k"),
            Err(GeminiError::RateLimited)
        );
        for (status, body) in [
            (500, Err(BodyError::TooLarge)),
            (503, Err(BodyError::Unreadable)),
            (400, ok(r#"{"error":{"message":"bad audio"}}"#)),
        ] {
            match interpret(status, body, "k") {
                Err(GeminiError::Other(m)) => assert!(m.contains(&format!("HTTP {status}")), "{m}"),
                other => panic!("{status}: {other:?}"),
            }
        }
    }

    #[test]
    fn key_test_is_strict() {
        // The verdict is the batch result: nothing but a recognized response is success.
        for (status, body) in [
            (400, ok("{}")),
            (500, ok("boom")),
            (200, ok("{}")),
            (200, ok("garbage")),
            (200, Err(BodyError::TooLarge)),
        ] {
            assert!(interpret(status, body, "k").is_err(), "{status}");
        }
        assert!(interpret(200, ok(SILENT), "k").is_ok());
        assert!(interpret(200, ok(OK), "k").is_ok());
    }

    struct Endless;
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(b'x');
            Ok(buf.len())
        }
    }

    struct Fails;
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("reset"))
        }
    }

    #[test]
    fn bodies_are_bounded() {
        assert_eq!(read_capped(&b"abc"[..], 3).unwrap(), b"abc");
        assert_eq!(read_capped(&b"abcd"[..], 3), Err(BodyError::TooLarge));
        // A body that never ends is stopped at the cap, not buffered.
        assert_eq!(read_capped(Endless, 1000), Err(BodyError::TooLarge));
        assert_eq!(read_capped(Fails, 10), Err(BodyError::Unreadable));
    }

    #[test]
    fn cancelled_before_sending_never_reaches_the_network() {
        let cancel = AtomicBool::new(true);
        let err = transcribe("k", &silence_wav(), None, &[], &cancel).unwrap_err();
        assert_eq!(err, cancelled());
        assert_eq!(test_key("k", &cancel), Err(cancelled()));
        assert_eq!(
            transcribe("", &[], None, &[], &cancel),
            Err(GeminiError::KeyMissing)
        );
    }

    #[test]
    fn silence_is_a_finished_wav() {
        let wav = silence_wav();
        assert_eq!(wav.len(), 44 + 32_000);
        assert_eq!(wav[..44], crate::audio::header(32_000));
    }
}
