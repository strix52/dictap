//! Gemini Live streaming transcription over a WebSocket. Runs on its own thread while
//! dictating; takes 100 ms frames from the capture queue.

use super::GeminiError;
use super::protocol::{self, Transcript};
use crate::capture::{LiveQueue, Pop};
use crate::event::{Event, LiveEvent};
use crate::key::Secret;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{HandshakeError, Message};

const HOST: &str = "generativelanguage.googleapis.com";
const PATH: &str = "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
/// Covers DNS, TCP, TLS, the WebSocket handshake and `setupComplete`.
const CONNECT_BUDGET: Duration = Duration::from_secs(15);
const FRAME: usize = 1600; // 100 ms at 16 kHz
const FRAMES_PER_PASS: usize = 10;
const KEEPALIVE: Duration = Duration::from_secs(15);
const FINISH_WAIT: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(10);
/// At most this often, the overlay gets the transcript so far.
const PARTIAL_EVERY: Duration = Duration::from_millis(100);

type OnText<'a> = &'a mut dyn FnMut(&Transcript) -> bool;

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

pub struct Params {
    pub key: Arc<Secret>,
    pub language: Option<String>,
    pub words: Vec<String>,
}

pub fn spawn(sid: u64, params: Params, queue: Arc<LiveQueue>, events: Sender<Event>) {
    std::thread::Builder::new()
        .name("live".into())
        .spawn(move || {
            let key = params.key.as_str().unwrap_or("");
            let mut tr = Transcript::default();
            let started = Instant::now();
            let mut shown = (String::new(), String::new());
            let mut last = Instant::now()
                .checked_sub(PARTIAL_EVERY)
                .unwrap_or_else(Instant::now);
            let partials = events.clone();
            // Returns false when throttled, so the caller tries again next pass.
            let mut on_text = |tr: &Transcript| {
                if last.elapsed() < PARTIAL_EVERY {
                    return false;
                }
                let (finals, interim) = tr.parts();
                if finals != shown.0 || interim != shown.1 {
                    last = Instant::now();
                    shown = (finals, interim.to_string());
                    let _ = partials.send(Event::LiveText {
                        sid,
                        finals: shown.0.clone(),
                        interim: shown.1.clone(),
                    });
                }
                true
            };
            let ev = match run(key, &params, &queue, &mut tr, &mut on_text) {
                Ok(()) => {
                    let (text, provisional) = tr.result();
                    log::info!(
                        "live {sid}: done in {} ms, {} chars, provisional={provisional}",
                        started.elapsed().as_millis(),
                        text.len()
                    );
                    LiveEvent::Done {
                        text,
                        provisional,
                        model: protocol::LIVE_MODEL,
                        error: None,
                    }
                }
                Err(error) => {
                    log::warn!("live {sid}: {error:?}");
                    LiveEvent::Failed {
                        error,
                        partial: tr.result().0,
                    }
                }
            };
            let _ = events.send(Event::Live { sid, ev });
        })
        .expect("spawn live thread");
}

fn remaining(deadline: Instant) -> Result<Duration, GeminiError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(GeminiError::Offline)
}

/// `ToSocketAddrs` has no timeout, so resolve on a helper thread.
fn resolve(deadline: Instant) -> Result<Vec<SocketAddr>, GeminiError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            (HOST, 443)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>),
        );
    });
    match rx.recv_timeout(remaining(deadline)?) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        _ => Err(GeminiError::Offline),
    }
}

fn connect(key: &str, p: &Params, deadline: Instant) -> Result<Ws, GeminiError> {
    let mut stream = None;
    for addr in resolve(deadline)? {
        if let Ok(s) = TcpStream::connect_timeout(&addr, remaining(deadline)?) {
            stream = Some(s);
            break;
        }
    }
    let stream = stream.ok_or(GeminiError::Offline)?;
    let left = remaining(deadline)?;
    let io = |_| GeminiError::Offline;
    stream.set_nodelay(true).map_err(io)?;
    stream.set_read_timeout(Some(left)).map_err(io)?;
    stream.set_write_timeout(Some(left)).map_err(io)?;

    let url = format!("wss://{HOST}{PATH}?key={key}");
    let mut ws = match tungstenite::client_tls(url, stream) {
        Ok((ws, _)) => ws,
        Err(HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
            let body = resp
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            return Err(GeminiError::from_http(resp.status().as_u16(), &body, key));
        }
        Err(HandshakeError::Failure(tungstenite::Error::Io(_) | tungstenite::Error::Tls(_))) => {
            return Err(GeminiError::Offline);
        }
        Err(e) => {
            return Err(GeminiError::Other(super::scrub(
                &format!("handshake: {e}"),
                key,
            )));
        }
    };
    ws.send(Message::text(protocol::live_setup(
        p.language.as_deref(),
        &p.words,
    )))
    .map_err(|e| ws_error(e, key))?;

    let MaybeTlsStream::NativeTls(tls) = ws.get_mut() else {
        return Err(GeminiError::Other("unexpected non-TLS stream".into()));
    };
    let tcp = tls.get_ref();
    tcp.set_read_timeout(None).map_err(io)?;
    tcp.set_write_timeout(None).map_err(io)?;
    tcp.set_nonblocking(true).map_err(io)?;
    Ok(ws)
}

fn ws_error(e: tungstenite::Error, key: &str) -> GeminiError {
    match e {
        tungstenite::Error::Io(_)
        | tungstenite::Error::Tls(_)
        | tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed => GeminiError::Offline,
        e => GeminiError::Other(super::scrub(&e.to_string(), key)),
    }
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

/// Sends a message; on a non-blocking socket a `WouldBlock` just means it's buffered.
fn send(ws: &mut Ws, msg: Message, key: &str) -> Result<(), GeminiError> {
    match ws.send(msg) {
        Err(e) if !would_block(&e) => Err(ws_error(e, key)),
        _ => Ok(()),
    }
}

/// Streams until the queue closes and the server finishes the turn. On error, `tr` keeps
/// whatever was transcribed so far.
fn run(
    key: &str,
    p: &Params,
    queue: &LiveQueue,
    tr: &mut Transcript,
    on_text: OnText,
) -> Result<(), GeminiError> {
    if key.is_empty() {
        return Err(GeminiError::KeyMissing);
    }
    let deadline = Instant::now() + CONNECT_BUDGET;
    let mut ws = connect(key, p, deadline)?;
    let result = stream(&mut ws, key, queue, tr, deadline, on_text);
    let _ = ws.close(None);
    let _ = ws.flush();
    result
}

fn stream(
    ws: &mut Ws,
    key: &str,
    queue: &LiveQueue,
    tr: &mut Transcript,
    setup_deadline: Instant,
    on_text: OnText,
) -> Result<(), GeminiError> {
    let mut setup = false;
    let mut end_sent: Option<Instant> = None;
    let mut last_ping = Instant::now();
    let mut awaiting_pong = false;
    let mut dirty = false;

    loop {
        // Read everything available.
        loop {
            let text = match ws.read() {
                Ok(Message::Text(t)) => t.to_string(),
                // The server sends JSON in binary frames too.
                Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(Message::Pong(_)) => {
                    awaiting_pong = false;
                    continue;
                }
                Ok(Message::Close(frame)) => {
                    if end_sent.is_some() {
                        return Ok(());
                    }
                    return Err(match frame {
                        Some(f) => GeminiError::from_close(u16::from(f.code), &f.reason, key),
                        None => GeminiError::Offline,
                    });
                }
                Ok(_) => continue,
                Err(e) if would_block(&e) => break,
                Err(e) => {
                    return if end_sent.is_some() {
                        Ok(())
                    } else {
                        Err(ws_error(e, key))
                    };
                }
            };
            if let Some(m) = protocol::parse_server(&text) {
                setup |= m.setup_complete;
                tr.apply(&m);
                dirty |= m.interim.is_some() || m.final_text.is_some();
            }
        }
        if dirty && on_text(tr) {
            dirty = false;
        }

        if let Some(at) = end_sent {
            if !tr.turn_open || at.elapsed() > FINISH_WAIT {
                return Ok(());
            }
            std::thread::sleep(TICK);
        } else if setup {
            for i in 0..FRAMES_PER_PASS {
                match queue.pop(FRAME, if i == 0 { TICK } else { Duration::ZERO }) {
                    Pop::Data(frame) => {
                        tr.sent_audio(&frame);
                        send(ws, Message::text(protocol::live_audio(&frame)), key)?;
                    }
                    Pop::Empty => break,
                    Pop::Closed => {
                        send(ws, Message::text(protocol::LIVE_END), key)?;
                        end_sent = Some(Instant::now());
                        break;
                    }
                    Pop::Overflowed => {
                        return Err(GeminiError::Other("Live fell behind".into()));
                    }
                }
            }
        } else {
            if Instant::now() > setup_deadline {
                return Err(GeminiError::Offline);
            }
            std::thread::sleep(TICK);
        }

        match ws.flush() {
            Err(e) if !would_block(&e) => return Err(ws_error(e, key)),
            _ => {}
        }
        if last_ping.elapsed() > KEEPALIVE {
            if awaiting_pong {
                return Err(GeminiError::Offline);
            }
            send(ws, Message::Ping(Default::default()), key)?;
            awaiting_pong = true;
            last_ping = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real network test with dictap's saved key (in memory only, never printed) and a
    /// TTS clip at probes/speech/hello.wav (16 kHz mono PCM).
    #[test]
    #[ignore]
    fn live_and_batch_real() {
        let key = Arc::new(crate::key::load().expect("key in Credential Manager"));
        let k = key.as_str().unwrap();
        let wav = std::fs::read("probes/speech/hello.wav").unwrap();
        let data = wav.windows(4).position(|w| w == b"data").unwrap() + 8;
        let samples: Vec<i16> = wav[data..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();

        let queue = Arc::new(LiveQueue::default());
        let q = queue.clone();
        let feeder = std::thread::spawn(move || {
            for chunk in samples.chunks(1600) {
                q.push(chunk);
                std::thread::sleep(Duration::from_millis(100));
            }
            q.close();
        });
        let params = Params {
            key: key.clone(),
            language: Some("en-GB".into()),
            words: vec!["dictap".into()],
        };
        let mut tr = Transcript::default();
        let t = Instant::now();
        let r = run(k, &params, &queue, &mut tr, &mut |_| true);
        feeder.join().unwrap();
        eprintln!(
            "live: {r:?} in {} ms -> {:?}",
            t.elapsed().as_millis(),
            tr.result()
        );
        assert!(r.is_ok());

        let t = Instant::now();
        let b = super::super::batch::transcribe(k, &wav, Some("en-GB"), &params.words);
        eprintln!("batch: {} ms -> {b:?}", t.elapsed().as_millis());
        assert!(b.is_ok());
        eprintln!("key test: {:?}", super::super::batch::test_key(k));
        eprintln!(
            "bad key test: {:?}",
            super::super::batch::test_key("AIzaInvalidKeyForTesting000000000000")
        );
    }
}
