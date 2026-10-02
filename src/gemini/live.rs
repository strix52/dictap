//! Gemini Live streaming transcription over a WebSocket. Runs on its own thread while
//! dictating; takes 100 ms frames from the capture queue.
//!
//! The thread's last act is the terminal `Event::Live`, sent after the socket is dropped, so
//! the core can treat that event as "the Live slot is free".

use super::GeminiError;
use super::protocol::{self, Transcript};
use crate::capture::{LiveQueue, Pop};
use crate::event::{Event, SessionId};
use crate::key::Secret;
use crate::outcome::{Failure, TranscriptOutcome};
use std::collections::HashSet;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tungstenite::protocol::{WebSocket, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{HandshakeError, Message};

const HOST: &str = "generativelanguage.googleapis.com";
const PATH: &str = "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
/// Covers DNS, TCP, TLS, the WebSocket handshake and `setupComplete`.
const CONNECT_TOTAL: Duration = Duration::from_secs(15);
/// One address lookup, or one TCP connect attempt, may take at most this long.
const ADDRESS_MAX: Duration = Duration::from_secs(3);
/// Kept back from the connect budget for TLS, the handshake and `setupComplete`: a TCP
/// attempt that would eat into it is not started.
const HANDSHAKE_RESERVE: Duration = Duration::from_secs(5);
const FRAME: usize = 1600; // 100 ms at 16 kHz
const FRAMES_PER_PASS: usize = 10;
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long the end-of-stream message may take to leave this machine.
const END_DRAIN_WAIT: Duration = Duration::from_secs(5);
/// After the end of stream has been flushed, how long to wait for more transcript before
/// giving up on a server that neither closes nor sends `generationComplete`.
const FINAL_WAIT: Duration = Duration::from_secs(3);
/// Any other write that stays unfinished this long means the connection is dead.
const WRITE_STALL: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(10);
/// At most this often, the overlay gets the transcript so far.
const PARTIAL_EVERY: Duration = Duration::from_millis(100);
/// Successful lookups are reused this long, so a burst of dictations asks DNS once.
const DNS_CACHE: Duration = Duration::from_secs(30);
/// Server messages handled per pass, so a flood cannot starve sending.
const READS_PER_PASS: usize = 256;

type OnText<'a> = &'a mut dyn FnMut(&Transcript) -> bool;

pub struct Params {
    pub key: Arc<Secret>,
    pub language: Option<String>,
    pub words: Vec<String>,
    /// Set by the core to abandon this stream (cancel, lock, quit).
    pub cancel: Arc<AtomicBool>,
}

/// Starts the Live thread. `Err` means no thread exists and no event will follow.
pub fn spawn(
    session: SessionId,
    params: Params,
    queue: Arc<LiveQueue>,
    events: Sender<Event>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("live".into())
        .spawn(move || {
            let outcome = work(session, &params, &queue, &events);
            let _ = events.send(Event::Live { session, outcome });
        })
        .map(|_| ())
}

fn work(
    session: SessionId,
    params: &Params,
    queue: &LiveQueue,
    events: &Sender<Event>,
) -> TranscriptOutcome {
    let key = params.key.as_str().unwrap_or("");
    let mut tr = Transcript::default();
    let started = Instant::now();
    let mut shown = (String::new(), String::new());
    let mut last = Instant::now()
        .checked_sub(PARTIAL_EVERY)
        .unwrap_or_else(Instant::now);
    // Returns false when throttled, so the caller tries again next pass.
    let mut on_text = |tr: &Transcript| {
        if last.elapsed() < PARTIAL_EVERY {
            return false;
        }
        let (finals, interim) = tr.parts();
        if finals != shown.0 || interim != shown.1 {
            last = Instant::now();
            shown = (finals, interim.to_string());
            let _ = events.send(Event::LiveText {
                session,
                finals: shown.0.clone(),
                interim: shown.1.clone(),
            });
        }
        true
    };
    let end = run(key, params, queue, &mut tr, &mut on_text);
    let outcome = tr.outcome(end);
    log::info!(
        "live {}: {} ms, outcome {}",
        session.0,
        started.elapsed().as_millis(),
        match &outcome {
            TranscriptOutcome::Complete { .. } => "complete".to_string(),
            TranscriptOutcome::Empty { .. } => "empty".to_string(),
            TranscriptOutcome::Incomplete { text, reason, .. } =>
                format!("incomplete ({} chars): {reason}", text.len()),
            TranscriptOutcome::Failed { reason } => format!("failed: {reason}"),
        }
    );
    outcome
}

// ---------------------------------------------------------------- time

trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Where the audio frames come from; the capture queue in production.
trait FrameSource {
    fn pop(&self, frame: usize, timeout: Duration) -> Pop;
}

impl FrameSource for LiveQueue {
    fn pop(&self, frame: usize, timeout: Duration) -> Pop {
        LiveQueue::pop(self, frame, timeout)
    }
}

// ---------------------------------------------------------------- resolving

/// One lookup at a time, process-wide, and a short-lived cache of the last success. The
/// lookup has no timeout of its own, so it runs on a helper thread that callers wait on; a
/// caller that gives up leaves the helper to finish for the next one instead of piling up
/// another thread behind a hung resolver.
struct Resolver {
    lookup: Arc<dyn Fn() -> io::Result<Vec<SocketAddr>> + Send + Sync>,
    state: Mutex<ResolveState>,
    done: Condvar,
}

#[derive(Default)]
struct ResolveState {
    cache: Option<(Instant, Vec<SocketAddr>)>,
    inflight: bool,
}

static RESOLVER: LazyLock<Arc<Resolver>> = LazyLock::new(|| {
    Arc::new(Resolver::new(Arc::new(|| {
        (HOST, 443)
            .to_socket_addrs()
            .map(Iterator::collect::<Vec<_>>)
    })))
});

/// Order kept, duplicates (the same address from several records) dropped.
fn dedup(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    addrs.into_iter().filter(|a| seen.insert(*a)).collect()
}

impl Resolver {
    fn new(lookup: Arc<dyn Fn() -> io::Result<Vec<SocketAddr>> + Send + Sync>) -> Resolver {
        Resolver {
            lookup,
            state: Mutex::new(ResolveState::default()),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ResolveState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(s: &ResolveState) -> Option<Vec<SocketAddr>> {
        s.cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < DNS_CACHE)
            .map(|(_, a)| a.clone())
    }

    fn resolve(self: &Arc<Self>, wait: Duration) -> Result<Vec<SocketAddr>, GeminiError> {
        let deadline = Instant::now() + wait;
        let mut s = self.lock();
        if let Some(a) = Self::fresh(&s) {
            return Ok(a);
        }
        if !s.inflight {
            s.inflight = true;
            let me = Arc::clone(self);
            let started = std::thread::Builder::new()
                .name("dns".into())
                .spawn(move || {
                    let found = (me.lookup)().map(dedup);
                    let mut s = me.lock();
                    if let Ok(a) = found
                        && !a.is_empty()
                    {
                        s.cache = Some((Instant::now(), a));
                    }
                    s.inflight = false;
                    me.done.notify_all();
                });
            if started.is_err() {
                s.inflight = false;
                return Err(GeminiError::Offline);
            }
        }
        loop {
            if !s.inflight {
                // Whoever finished last left the answer (or nothing, if it failed).
                return Self::fresh(&s).ok_or(GeminiError::Offline);
            }
            let Some(left) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                return Err(GeminiError::Offline);
            };
            s = self
                .done
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

// ---------------------------------------------------------------- connecting

/// A TCP stream whose every read and write is bounded by the connect deadline, so a slow TLS
/// or WebSocket handshake cannot outlive the budget however many round trips it takes. Once
/// connected the deadline is dropped and the socket goes non-blocking.
#[derive(Debug)]
struct Bounded {
    tcp: TcpStream,
    deadline: Option<Instant>,
}

impl Bounded {
    fn left(&self) -> io::Result<Option<Duration>> {
        match self.deadline {
            None => Ok(None),
            Some(d) => d
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .map(Some)
                .ok_or_else(|| io::Error::new(ErrorKind::TimedOut, "connect budget spent")),
        }
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(left) = self.left()? {
            self.tcp.set_read_timeout(Some(left))?;
        }
        self.tcp.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(left) = self.left()? {
            self.tcp.set_write_timeout(Some(left))?;
        }
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

type Ws = WebSocket<MaybeTlsStream<Bounded>>;

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
}

/// Small writes go out as they are made; a stalled socket may hold at most a megabyte, which
/// is hundreds of frames, before the sender is told to stop.
fn ws_config() -> WebSocketConfig {
    let mut c = WebSocketConfig::default();
    c.write_buffer_size = 0;
    c.max_write_buffer_size = 1 << 20;
    c.max_message_size = Some(2 << 20);
    c.max_frame_size = Some(2 << 20);
    c
}

fn tcp_connect(deadline: Instant) -> Result<TcpStream, GeminiError> {
    let wait = remaining(deadline)
        .ok_or(GeminiError::Offline)?
        .min(ADDRESS_MAX);
    let addrs = RESOLVER.resolve(wait)?;
    try_addresses(
        addrs,
        || remaining(deadline),
        |addr, budget| TcpStream::connect_timeout(&addr, budget),
    )
}

fn try_addresses<T>(
    addrs: Vec<SocketAddr>,
    mut remaining: impl FnMut() -> Option<Duration>,
    mut connect: impl FnMut(SocketAddr, Duration) -> io::Result<T>,
) -> Result<T, GeminiError> {
    for addr in addrs {
        // Leave the handshake its reserve; each attempt is also capped on its own.
        let Some(budget) =
            remaining().and_then(|r| r.checked_sub(HANDSHAKE_RESERVE).filter(|d| !d.is_zero()))
        else {
            break;
        };
        if let Ok(s) = connect(addr, budget.min(ADDRESS_MAX)) {
            return Ok(s);
        }
    }
    Err(GeminiError::Offline)
}

fn connect(key: &str, p: &Params, deadline: Instant) -> Result<Ws, GeminiError> {
    let tcp = tcp_connect(deadline)?;
    let io = |_| GeminiError::Offline;
    tcp.set_nodelay(true).map_err(io)?;
    let stream = Bounded {
        tcp,
        deadline: Some(deadline),
    };

    let url = format!("wss://{HOST}{PATH}?key={key}");
    let mut ws = match tungstenite::client_tls_with_config(url, stream, Some(ws_config()), None) {
        Ok((ws, _)) => ws,
        Err(HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
            let body = resp
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            return Err(GeminiError::from_http(resp.status().as_u16(), &body, key));
        }
        Err(HandshakeError::Failure(tungstenite::Error::Io(_) | tungstenite::Error::Tls(_)))
        | Err(HandshakeError::Interrupted(_)) => {
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
    .map_err(|e| match ws_failure(e, key) {
        Failure::Provider(g) => g,
        other => GeminiError::Other(other.to_string()),
    })?;

    let MaybeTlsStream::NativeTls(tls) = ws.get_mut() else {
        return Err(GeminiError::Other("unexpected non-TLS stream".into()));
    };
    let bounded = tls.get_mut();
    bounded.deadline = None;
    bounded.tcp.set_read_timeout(None).map_err(io)?;
    bounded.tcp.set_write_timeout(None).map_err(io)?;
    bounded.tcp.set_nonblocking(true).map_err(io)?;
    Ok(ws)
}

// ---------------------------------------------------------------- streaming

fn ws_failure(e: tungstenite::Error, key: &str) -> Failure {
    match e {
        tungstenite::Error::Io(_)
        | tungstenite::Error::Tls(_)
        | tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed => Failure::Provider(GeminiError::Offline),
        tungstenite::Error::Capacity(_) => Failure::TooLarge,
        e => Failure::Protocol(super::scrub(&e.to_string(), key)),
    }
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

/// Streams until the queue closes and the server finishes or the wait runs out. `tr` keeps
/// whatever was transcribed, whether this returns `Ok` or not.
fn run(
    key: &str,
    p: &Params,
    queue: &LiveQueue,
    tr: &mut Transcript,
    on_text: OnText,
) -> Result<(), Failure> {
    if key.is_empty() {
        return Err(Failure::Provider(GeminiError::KeyMissing));
    }
    let deadline = Instant::now() + CONNECT_TOTAL;
    let mut ws = connect(key, p, deadline).map_err(Failure::Provider)?;
    let result = drive(
        &mut ws,
        key,
        queue,
        tr,
        &SystemClock,
        &p.cancel,
        deadline,
        on_text,
    );
    let _ = ws.close(None);
    let _ = ws.flush();
    // `ws` (and the socket) drop here, before the caller reports the outcome.
    result
}

/// Something the socket has been handed but may not have fully sent, or been refused.
enum Held {
    Audio(Vec<i16>),
    End,
    Ping,
}

impl Held {
    fn message(&self) -> Message {
        match self {
            Held::Audio(s) => Message::text(protocol::live_audio(s)),
            Held::End => Message::text(protocol::LIVE_END),
            Held::Ping => Message::Ping(Default::default()),
        }
    }
}

struct Stream<'a, S: Read + Write> {
    ws: &'a mut WebSocket<S>,
    key: &'a str,
    tr: &'a mut Transcript,
    clock: &'a dyn Clock,
    on_text: OnText<'a>,
    setup: bool,
    dirty: bool,
    awaiting_pong: bool,
    last_ping: Instant,
    /// The socket accepted a message but part of it is still unsent: nothing else is written
    /// (and no audio is taken from the queue) until a flush finishes it.
    flushing: bool,
    /// The socket refused a message (write buffer full); it is offered again after a flush.
    held: Option<Held>,
    pending_since: Option<Instant>,
    /// When the end of stream was decided on. It is queued exactly once.
    end_queued: Option<Instant>,
    end_accepted: bool,
    /// Set once the end of stream has been flushed; the final wait runs from here.
    final_deadline: Option<Instant>,
}

#[allow(clippy::too_many_arguments)]
fn drive<S: Read + Write>(
    ws: &mut WebSocket<S>,
    key: &str,
    src: &dyn FrameSource,
    tr: &mut Transcript,
    clock: &dyn Clock,
    cancel: &AtomicBool,
    setup_deadline: Instant,
    on_text: OnText,
) -> Result<(), Failure> {
    let now = clock.now();
    let mut st = Stream {
        ws,
        key,
        tr,
        clock,
        on_text,
        setup: false,
        dirty: false,
        awaiting_pong: false,
        last_ping: now,
        flushing: false,
        held: None,
        pending_since: None,
        end_queued: None,
        end_accepted: false,
        final_deadline: None,
    };
    st.run(src, cancel, setup_deadline)
}

impl<S: Read + Write> Stream<'_, S> {
    fn pending(&self) -> bool {
        self.flushing || self.held.is_some()
    }

    /// Offers one message to the socket and records what became of it.
    fn send(&mut self, h: Held) -> Result<(), Failure> {
        match self.ws.write(h.message()) {
            Ok(()) => match self.ws.flush() {
                Ok(()) => {}
                Err(e) if would_block(&e) => self.flushing = true,
                Err(e) => return Err(ws_failure(e, self.key)),
            },
            // Accepted into the write buffer, partly or wholly unsent: do not offer it again.
            Err(e) if would_block(&e) => self.flushing = true,
            // Not accepted at all: keep it and try again once the buffer has drained.
            Err(tungstenite::Error::WriteBufferFull(_)) => {
                self.held = Some(h);
                return Ok(());
            }
            Err(e) => return Err(ws_failure(e, self.key)),
        }
        if matches!(h, Held::End) {
            self.end_accepted = true;
            // Anything the server says from here on is about the whole stream.
            self.tr.mark_end_flushed();
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), Failure> {
        if self.flushing {
            match self.ws.flush() {
                Ok(()) => self.flushing = false,
                Err(e) if would_block(&e) => {}
                Err(e) => return Err(ws_failure(e, self.key)),
            }
        }
        if !self.flushing
            && let Some(h) = self.held.take()
        {
            self.send(h)?;
        }
        Ok(())
    }

    /// Reads what is available. `Ok(true)`: the server closed the stream after our end.
    fn read_all(&mut self) -> Result<bool, Failure> {
        for _ in 0..READS_PER_PASS {
            let text = match self.ws.read() {
                Ok(Message::Text(t)) => t.to_string(),
                // The server sends JSON in binary frames too.
                Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(Message::Pong(_)) => {
                    self.awaiting_pong = false;
                    continue;
                }
                Ok(Message::Close(frame)) => {
                    let normal = frame
                        .as_ref()
                        .is_some_and(|f| matches!(u16::from(f.code), 1000 | 1001));
                    if normal && self.end_accepted && !self.pending() {
                        return Ok(true);
                    }
                    return Err(match frame {
                        Some(f) => Failure::Provider(GeminiError::from_close(
                            u16::from(f.code),
                            &f.reason,
                            self.key,
                        )),
                        None => Failure::Provider(GeminiError::Offline),
                    });
                }
                Ok(_) => continue,
                Err(e) if would_block(&e) => break,
                Err(e) => return Err(ws_failure(e, self.key)),
            };
            let m = protocol::parse_server(&text)
                .map_err(|e| Failure::Protocol(format!("Live message: {e}")))?;
            self.setup |= m.setup_complete;
            self.tr.apply(&m);
            self.dirty |= m.interim.is_some() || m.final_text.is_some();
        }
        Ok(false)
    }

    fn run(
        &mut self,
        src: &dyn FrameSource,
        cancel: &AtomicBool,
        setup_deadline: Instant,
    ) -> Result<(), Failure> {
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            if self.read_all()? {
                return Ok(());
            }
            if self.dirty && (self.on_text)(self.tr) {
                self.dirty = false;
            }

            self.drain()?;
            let now = self.clock.now();
            if self.pending() {
                let since = *self.pending_since.get_or_insert(now);
                if now.saturating_duration_since(since) > WRITE_STALL {
                    return Err(Failure::Timeout);
                }
            } else {
                self.pending_since = None;
                if self.end_accepted && self.final_deadline.is_none() {
                    self.final_deadline = Some(now + FINAL_WAIT);
                }
            }

            if let Some(at) = self.end_queued {
                match self.final_deadline {
                    Some(d) => {
                        if now >= d {
                            return Ok(());
                        }
                    }
                    None if now.saturating_duration_since(at) > END_DRAIN_WAIT => {
                        return Err(Failure::Timeout);
                    }
                    None => {}
                }
                self.clock.sleep(TICK);
            } else if !self.setup {
                if now > setup_deadline {
                    return Err(Failure::Provider(GeminiError::Offline));
                }
                self.clock.sleep(TICK);
            } else if self.pending() {
                self.clock.sleep(TICK);
            } else {
                self.pump_audio(src)?;
            }

            let now = self.clock.now();
            if now.saturating_duration_since(self.last_ping) > KEEPALIVE && !self.pending() {
                if self.awaiting_pong {
                    return Err(Failure::Provider(GeminiError::Offline));
                }
                self.send(Held::Ping)?;
                self.awaiting_pong = true;
                self.last_ping = now;
            }
        }
    }

    /// Sends up to a pass worth of frames, stopping at the first one the socket did not
    /// fully take.
    fn pump_audio(&mut self, src: &dyn FrameSource) -> Result<(), Failure> {
        for i in 0..FRAMES_PER_PASS {
            match src.pop(FRAME, if i == 0 { TICK } else { Duration::ZERO }) {
                Pop::Data(frame) => {
                    self.send(Held::Audio(frame))?;
                    if self.pending() {
                        break;
                    }
                }
                Pop::Empty => break,
                Pop::Closed => {
                    self.end_queued = Some(self.clock.now());
                    self.send(Held::End)?;
                    break;
                }
                Pop::Overflowed => {
                    return Err(Failure::Internal("Live fell behind".into()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    // ------------------------------------------------------------ scripted transport

    #[derive(Default)]
    struct Wire {
        inbound: VecDeque<Vec<u8>>,
        /// Sent to `inbound` once the client's end-of-stream message has been written.
        after_end: Vec<Vec<u8>>,
        outbound: Vec<u8>,
        /// `None`: every write is accepted. `Some(n)`: only n more bytes, then `WouldBlock`.
        budget: Option<usize>,
        blocked_hits: u32,
        /// After this many refused writes the budget is lifted.
        unblock_after: Option<u32>,
    }

    #[derive(Clone, Default)]
    struct Script(Rc<RefCell<Wire>>);

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut w = self.0.borrow_mut();
            let Some(mut next) = w.inbound.pop_front() else {
                return Err(io::Error::from(ErrorKind::WouldBlock));
            };
            let n = next.len().min(buf.len());
            buf[..n].copy_from_slice(&next[..n]);
            if n < next.len() {
                w.inbound.push_front(next.split_off(n));
            }
            Ok(n)
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut w = self.0.borrow_mut();
            let n = match w.budget {
                None => buf.len(),
                Some(left) => buf.len().min(left),
            };
            if n == 0 {
                w.blocked_hits += 1;
                if w.unblock_after.is_some_and(|k| w.blocked_hits >= k) {
                    w.budget = None;
                }
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            if let Some(left) = w.budget.as_mut() {
                *left -= n;
            }
            w.outbound.extend_from_slice(&buf[..n]);
            if !w.after_end.is_empty() && sent(&w.outbound).contains(&Sent::End) {
                let more = std::mem::take(&mut w.after_end);
                w.inbound.extend(more);
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, PartialEq)]
    enum Sent {
        Audio,
        End,
        Ping,
        Close,
        Other,
    }

    /// Decodes the client's (masked) frames; a trailing partial frame is ignored.
    fn sent(bytes: &[u8]) -> Vec<Sent> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 <= bytes.len() {
            let op = bytes[i] & 0x0f;
            let mut len = usize::from(bytes[i + 1] & 0x7f);
            let mut at = i + 2;
            if len == 126 {
                if at + 2 > bytes.len() {
                    break;
                }
                len = usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]));
                at += 2;
            }
            let masked = bytes[i + 1] & 0x80 != 0;
            let mask_at = at;
            if masked {
                at += 4;
            }
            if at + len > bytes.len() {
                break;
            }
            let payload: Vec<u8> = bytes[at..at + len]
                .iter()
                .enumerate()
                .map(|(k, b)| {
                    if masked {
                        b ^ bytes[mask_at + k % 4]
                    } else {
                        *b
                    }
                })
                .collect();
            out.push(match op {
                1 => {
                    let t = String::from_utf8_lossy(&payload);
                    if t.contains("audioStreamEnd") {
                        Sent::End
                    } else if t.contains("\"audio\"") {
                        Sent::Audio
                    } else {
                        Sent::Other
                    }
                }
                9 => Sent::Ping,
                8 => Sent::Close,
                _ => Sent::Other,
            });
            i = at + len;
        }
        out
    }

    fn server_text(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        assert!(b.len() < 126);
        let mut v = vec![0x81, b.len() as u8];
        v.extend_from_slice(b);
        v
    }

    fn server_close(code: u16, reason: &str) -> Vec<u8> {
        let mut v = vec![0x88, (2 + reason.len()) as u8];
        v.extend_from_slice(&code.to_be_bytes());
        v.extend_from_slice(reason.as_bytes());
        v
    }

    const SETUP: &str = r#"{"setupComplete":{}}"#;
    const HELLO: &str = r#"{"serverContent":{"inputTranscription":{"text":"Hello."}}}"#;
    const GEN: &str = r#"{"serverContent":{"generationComplete":true}}"#;

    struct FakeClock {
        base: Instant,
        off: Cell<Duration>,
    }

    impl FakeClock {
        fn new() -> FakeClock {
            FakeClock {
                base: Instant::now(),
                off: Cell::new(Duration::ZERO),
            }
        }
        fn elapsed(&self) -> Duration {
            self.off.get()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + self.off.get()
        }
        fn sleep(&self, d: Duration) {
            self.off.set(self.off.get() + d);
        }
    }

    /// Hands out scripted pops; once exhausted it behaves like an idle queue, spending the
    /// fake clock instead of waiting.
    struct Frames {
        clock: Rc<FakeClock>,
        script: RefCell<VecDeque<Pop>>,
        /// Runs before each pop with the number of pops so far.
        hook: RefCell<Box<dyn FnMut(usize)>>,
        pops: Cell<usize>,
    }

    impl Frames {
        fn new(clock: &Rc<FakeClock>, script: Vec<Pop>) -> Frames {
            Frames {
                clock: clock.clone(),
                script: RefCell::new(script.into()),
                hook: RefCell::new(Box::new(|_| {})),
                pops: Cell::new(0),
            }
        }
    }

    impl FrameSource for Frames {
        fn pop(&self, _frame: usize, timeout: Duration) -> Pop {
            let n = self.pops.get();
            self.pops.set(n + 1);
            (self.hook.borrow_mut())(n);
            self.script.borrow_mut().pop_front().unwrap_or_else(|| {
                self.clock.sleep(timeout.max(TICK));
                Pop::Empty
            })
        }
    }

    fn audio() -> Pop {
        Pop::Data(vec![1, 2, 3, 4])
    }

    struct Rig {
        wire: Script,
        ws: WebSocket<Script>,
        clock: Rc<FakeClock>,
        tr: Transcript,
    }

    fn rig() -> Rig {
        let wire = Script::default();
        let ws = WebSocket::from_raw_socket(
            wire.clone(),
            tungstenite::protocol::Role::Client,
            Some(ws_config()),
        );
        Rig {
            wire,
            ws,
            clock: Rc::new(FakeClock::new()),
            tr: Transcript::default(),
        }
    }

    impl Rig {
        fn serve(&self, msgs: &[&str]) {
            let mut w = self.wire.0.borrow_mut();
            for m in msgs {
                w.inbound.push_back(server_text(m));
            }
        }
        fn after_end(&self, msgs: &[&str]) {
            let mut w = self.wire.0.borrow_mut();
            for m in msgs {
                w.after_end.push(server_text(m));
            }
        }
        fn go(&mut self, src: &Frames, cancel: &AtomicBool) -> Result<(), Failure> {
            let deadline = self.clock.now() + Duration::from_secs(5);
            drive(
                &mut self.ws,
                "KEY",
                src,
                &mut self.tr,
                &*self.clock,
                cancel,
                deadline,
                &mut |_| true,
            )
        }
        fn frames(&self) -> Vec<Sent> {
            sent(&self.wire.0.borrow().outbound)
        }
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    // ------------------------------------------------------------ transport tests

    #[test]
    fn streams_audio_then_ends_once_and_waits_for_unordered_transcription() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        r.after_end(&[GEN]);
        let src = Frames::new(&r.clock, vec![audio(), audio(), audio(), Pop::Closed]);
        let res = r.go(&src, &no_cancel());
        assert_eq!(res, Ok(()));
        let f = r.frames();
        assert_eq!(f.iter().filter(|s| **s == Sent::Audio).count(), 3);
        assert_eq!(f.iter().filter(|s| **s == Sent::End).count(), 1, "{f:?}");
        assert!(
            r.clock.elapsed() >= FINAL_WAIT,
            "generation completion cannot end the transcription wait"
        );
        // The transcript is never complete without evidence, however cleanly it ended.
        assert_eq!(
            r.tr.outcome(res),
            TranscriptOutcome::Incomplete {
                text: "Hello.".into(),
                model: protocol::LIVE_MODEL,
                reason: Failure::Unconfirmed
            }
        );
    }

    #[test]
    fn without_generation_complete_the_final_wait_runs_after_the_end_flush() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        let src = Frames::new(&r.clock, vec![audio(), Pop::Closed]);
        assert_eq!(r.go(&src, &no_cancel()), Ok(()));
        let waited = r.clock.elapsed();
        assert!(waited >= FINAL_WAIT, "{waited:?}");
        assert!(waited < FINAL_WAIT + Duration::from_secs(1), "{waited:?}");
    }

    #[test]
    fn generation_complete_seen_before_the_end_does_not_cut_the_wait_short() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO, GEN]);
        let src = Frames::new(&r.clock, vec![audio(), Pop::Closed]);
        assert_eq!(r.go(&src, &no_cancel()), Ok(()));
        assert!(r.clock.elapsed() >= FINAL_WAIT);
    }

    #[test]
    fn server_close_after_the_end_is_a_normal_finish() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        r.wire
            .0
            .borrow_mut()
            .after_end
            .push(server_close(1000, "bye"));
        let src = Frames::new(&r.clock, vec![audio(), Pop::Closed]);
        assert_eq!(r.go(&src, &no_cancel()), Ok(()));
        assert!(r.clock.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn server_close_before_the_end_keeps_the_text_and_maps_the_code() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        r.wire
            .0
            .borrow_mut()
            .inbound
            .push_back(server_close(1008, "bad key"));
        let src = Frames::new(&r.clock, vec![audio()]);
        let res = r.go(&src, &no_cancel());
        assert_eq!(res, Err(Failure::Provider(GeminiError::KeyInvalid)));
        assert!(matches!(
            r.tr.outcome(res),
            TranscriptOutcome::Incomplete {
                reason: Failure::Provider(GeminiError::KeyInvalid),
                ..
            }
        ));
    }

    #[test]
    fn a_partially_written_frame_stops_the_queue_being_drained_until_it_flushes() {
        let mut r = rig();
        r.serve(&[SETUP]);
        let wire = r.wire.clone();
        let violations = Rc::new(Cell::new(0));
        let v = violations.clone();
        let src = Frames::new(
            &r.clock,
            vec![audio(), audio(), audio(), audio(), Pop::Closed],
        );
        *src.hook.borrow_mut() = Box::new(move |n| {
            let mut w = wire.0.borrow_mut();
            if n == 1 {
                // The second frame will be accepted only partway, then the socket blocks.
                w.budget = Some(7);
                w.unblock_after = Some(4);
            } else if n > 1 && w.budget == Some(0) {
                // A frame was taken from the queue while the previous one was still unsent.
                v.set(v.get() + 1);
            }
        });
        let res = r.go(&src, &no_cancel());
        assert_eq!(res, Ok(()));
        assert_eq!(
            violations.get(),
            0,
            "audio was popped behind an unsent frame"
        );
        let f = r.frames();
        assert_eq!(
            f.iter().filter(|s| **s == Sent::Audio).count(),
            4,
            "none lost, none duplicated: {f:?}"
        );
        assert_eq!(f.iter().filter(|s| **s == Sent::End).count(), 1);
    }

    #[test]
    fn a_stuck_end_of_stream_times_out_without_starting_the_final_wait() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        let wire = r.wire.clone();
        let src = Frames::new(&r.clock, vec![audio(), Pop::Closed]);
        *src.hook.borrow_mut() = Box::new(move |n| {
            if n == 1 {
                wire.0.borrow_mut().budget = Some(0);
            }
        });
        let res = r.go(&src, &no_cancel());
        assert_eq!(res, Err(Failure::Timeout));
        let waited = r.clock.elapsed();
        assert!(waited >= END_DRAIN_WAIT, "{waited:?}");
        assert!(
            waited < END_DRAIN_WAIT + FINAL_WAIT,
            "the final wait must not have run: {waited:?}"
        );
        assert!(matches!(
            r.tr.outcome(res),
            TranscriptOutcome::Incomplete {
                reason: Failure::Timeout,
                ..
            }
        ));
    }

    #[test]
    fn a_stalled_mid_stream_write_gives_up() {
        let mut r = rig();
        r.serve(&[SETUP]);
        let wire = r.wire.clone();
        let src = Frames::new(&r.clock, vec![audio(), audio()]);
        *src.hook.borrow_mut() = Box::new(move |n| {
            if n == 1 {
                wire.0.borrow_mut().budget = Some(3);
            }
        });
        assert_eq!(r.go(&src, &no_cancel()), Err(Failure::Timeout));
        assert!(r.clock.elapsed() >= WRITE_STALL);
    }

    #[test]
    fn a_missing_setup_is_offline_after_the_deadline() {
        let mut r = rig();
        let src = Frames::new(&r.clock, vec![]);
        assert_eq!(
            r.go(&src, &no_cancel()),
            Err(Failure::Provider(GeminiError::Offline))
        );
        assert!(r.frames().is_empty(), "no audio before setupComplete");
    }

    #[test]
    fn keepalive_pings_once_and_a_missing_pong_is_offline() {
        let mut r = rig();
        r.serve(&[SETUP]);
        let src = Frames::new(&r.clock, vec![]);
        assert_eq!(
            r.go(&src, &no_cancel()),
            Err(Failure::Provider(GeminiError::Offline))
        );
        let f = r.frames();
        assert_eq!(f.iter().filter(|s| **s == Sent::Ping).count(), 1, "{f:?}");
        assert!(r.clock.elapsed() >= KEEPALIVE * 2 - Duration::from_secs(1));
    }

    #[test]
    fn cancel_and_queue_overflow_end_the_stream() {
        let mut r = rig();
        r.serve(&[SETUP]);
        let src = Frames::new(&r.clock, vec![]);
        let cancel = AtomicBool::new(true);
        assert_eq!(r.go(&src, &cancel), Err(Failure::Cancelled));

        let mut r = rig();
        r.serve(&[SETUP]);
        let src = Frames::new(&r.clock, vec![audio(), Pop::Overflowed]);
        let res = r.go(&src, &no_cancel());
        assert!(matches!(res, Err(Failure::Internal(_))), "{res:?}");
        assert!(res.unwrap_err().wants_fallback(), "the WAV is still whole");
    }

    #[test]
    fn a_malformed_server_message_is_a_protocol_failure() {
        let mut r = rig();
        r.serve(&[SETUP, r#"{"serverContent":[]}"#]);
        let src = Frames::new(&r.clock, vec![audio()]);
        let res = r.go(&src, &no_cancel());
        assert!(matches!(res, Err(Failure::Protocol(_))), "{res:?}");
    }

    // ------------------------------------------------------------ resolver

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([203, 0, 113, last], port))
    }

    #[test]
    fn addresses_are_deduplicated_in_order() {
        let a = dedup(vec![addr(1, 443), addr(2, 443), addr(1, 443), addr(3, 443)]);
        assert_eq!(a, vec![addr(1, 443), addr(2, 443), addr(3, 443)]);
    }

    #[test]
    fn concurrent_resolves_share_one_lookup_and_the_cache_serves_the_next() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = calls.clone();
        let r = Arc::new(Resolver::new(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Ok(vec![addr(1, 443), addr(1, 443), addr(2, 443)])
        })));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let r = r.clone();
                std::thread::spawn(move || r.resolve(Duration::from_secs(5)))
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), Ok(vec![addr(1, 443), addr(2, 443)]));
        }
        assert_eq!(r.resolve(Duration::from_secs(1)).unwrap().len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_hung_lookup_times_out_without_a_second_helper() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = calls.clone();
        let r = Arc::new(Resolver::new(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            Ok(vec![addr(9, 443)])
        })));
        assert_eq!(
            r.resolve(Duration::from_millis(50)),
            Err(GeminiError::Offline)
        );
        // The next caller waits on the same lookup instead of starting another.
        assert_eq!(r.resolve(Duration::from_secs(3)), Ok(vec![addr(9, 443)]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_lookup_is_not_cached() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = calls.clone();
        let r = Arc::new(Resolver::new(Arc::new(move || {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(io::Error::other("down"))
            } else {
                Ok(vec![addr(5, 443)])
            }
        })));
        assert_eq!(r.resolve(Duration::from_secs(2)), Err(GeminiError::Offline));
        assert_eq!(r.resolve(Duration::from_secs(2)), Ok(vec![addr(5, 443)]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn websocket_config_is_bounded() {
        let c = ws_config();
        assert_eq!(c.write_buffer_size, 0);
        assert_eq!(c.max_write_buffer_size, 1 << 20);
        assert_eq!(c.max_message_size, Some(2 << 20));
        assert_eq!(c.max_frame_size, Some(2 << 20));
    }

    // ------------------------------------------------------------ real network (never run unattended)

    /// Real network test with dictap's saved key (in memory only, never printed) and a
    /// TTS clip at probes/speech/hello.wav (16 kHz mono PCM). Reads the saved credential and
    /// uploads audio to Gemini: it is `#[ignore]`d and must only be run by the owner, by name.
    #[test]
    #[ignore = "reads the saved API key and uploads audio to Gemini"]
    fn live_and_batch_real() {
        let key = Arc::new(crate::key::load().expect("key in Credential Manager"));
        let k = key.as_str().unwrap();
        let wav = std::fs::read(
            std::env::var_os("DICTAP_SYNTHETIC_WAV").expect("explicit synthetic fixture path"),
        )
        .unwrap();
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
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let mut tr = Transcript::default();
        let t = Instant::now();
        let r = run(k, &params, &queue, &mut tr, &mut |_| true);
        feeder.join().unwrap();
        eprintln!(
            "live: {r:?} in {} ms -> {:?}",
            t.elapsed().as_millis(),
            tr.text()
        );
        assert!(r.is_ok());

        let t = Instant::now();
        let b = super::super::batch::transcribe(
            k,
            &wav,
            Some("en-GB"),
            &params.words,
            &AtomicBool::new(false),
        );
        eprintln!("batch: {} ms -> {b:?}", t.elapsed().as_millis());
        assert!(b.is_ok());
    }
    #[test]
    fn abnormal_close_after_end_is_still_provider_failure() {
        let mut r = rig();
        r.serve(&[SETUP, HELLO]);
        r.wire
            .0
            .borrow_mut()
            .after_end
            .push(server_close(1011, "synthetic server failure"));
        let src = Frames::new(&r.clock, vec![audio(), Pop::Closed]);
        let result = r.go(&src, &no_cancel());
        assert!(result.is_err());
        assert!(matches!(
            r.tr.outcome(result),
            TranscriptOutcome::Incomplete {
                reason: Failure::Provider(_),
                ..
            }
        ));
    }
    #[test]
    fn a_stalled_address_leaves_budget_for_the_next_and_handshake() {
        let elapsed = Cell::new(Duration::ZERO);
        let tried = RefCell::new(Vec::new());
        let result = try_addresses(
            vec![addr(1, 443), addr(2, 443)],
            || Some(Duration::from_secs(15).saturating_sub(elapsed.get())),
            |endpoint, budget| {
                tried.borrow_mut().push((endpoint, budget));
                if endpoint == addr(1, 443) {
                    elapsed.set(elapsed.get() + budget);
                    Err(io::Error::other("stalled"))
                } else {
                    Ok(endpoint)
                }
            },
        );
        assert_eq!(result, Ok(addr(2, 443)));
        assert_eq!(tried.borrow().len(), 2);
        assert_eq!(tried.borrow()[0].1, ADDRESS_MAX);
        assert!(Duration::from_secs(15) - elapsed.get() >= HANDSHAKE_RESERVE);
    }
    #[test]
    fn address_attempts_never_spend_the_handshake_reserve() {
        let elapsed = Cell::new(Duration::ZERO);
        let result: Result<(), _> = try_addresses(
            (1..10).map(|n| addr(n, 443)).collect(),
            || Some(Duration::from_secs(15).saturating_sub(elapsed.get())),
            |_, budget| {
                elapsed.set(elapsed.get() + budget);
                Err(io::Error::other("stalled"))
            },
        );
        assert!(result.is_err());
        assert_eq!(elapsed.get(), Duration::from_secs(10));
    }
}
