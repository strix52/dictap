//! The core loop: the single owner of dictation state, the write DB connection, settings and
//! the API key. Everything arrives as an `Event`; everything the core does to the outside world
//! leaves as an `Effect` through a `Runtime`, so the whole machine can be driven in tests with a
//! fake clock and recorded effects.
//!
//! One dictation moves Idle → Starting → Recording → Finishing → committed. Finishing needs two
//! independent facts before it can commit: the capture's terminal report (so the WAV is closed
//! and its state known) and a provider outcome (Live, or Live then batch). Cancelling never
//! renames an open WAV: an abandoned dictation whose capture is still running is parked in
//! `retiring` until the capture reports, and new recordings are refused meanwhile.

use crate::capture::{self, AudioState, Capture, CaptureReport, LiveQueue};
use crate::event::{
    Action, ActionFailure, ActionRequest, ActionResult, ActionSuccess, CaptureEvent, Event, JobId,
    PowerEvent, RequestId, SessionId,
};
use crate::gemini::protocol::{BATCH_MODEL, LIVE_MODEL};
use crate::gemini::{GeminiError, batch, live};
use crate::hotkey::Chord;
use crate::hud::{HudCommand, HudOwner, OwnedPresentation};
use crate::key::Secret;
use crate::outcome::{BatchText, Failure, TranscriptOutcome};
use crate::paste::{self, DeliveryGate, DeliveryPermission, PasteRequest, PasteTarget};
use crate::persist::{self, AudioFs, Commit, Dirs, FinishDisposition, RealFs};
use crate::settings::Settings;
use crate::store::Store;
use crate::win::overlay::Tone;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Gemini Live ends a session at 10 minutes; stop a little before so the last words come
/// back. The overlay counts down the final seconds.
const MAX_RECORDING: Duration = Duration::from_secs(9 * 60 + 45);
/// How long quit waits for a finishing dictation; its spool WAV is recovered next start.
const QUIT_WAIT: Duration = Duration::from_secs(5);
const NOTICE: Duration = Duration::from_secs(4);
/// How long after stopping to wait for Live's verdict before giving up (Live itself
/// finishes within seconds; this only catches a hang).
const FINISH_WAIT: Duration = Duration::from_secs(45);
/// Extra slack on top of batch's own request timeout.
const BATCH_SLACK: Duration = Duration::from_secs(20);
/// A second press within this long cancels a transcription.
const CANCEL_WINDOW: Duration = Duration::from_secs(3);
/// How long a Live fallback waits for the single batch worker before settling for what it has.
const BATCH_SLOT_WAIT: Duration = Duration::from_secs(5);
const KEEP_FILES: usize = 20;
const KEEP_BYTES: u64 = 200 << 20;
/// Batch requests in flight at once: fallback, Retry and the key check share one slot.
const MAX_BATCH_WORKERS: usize = 1;

pub struct Paths {
    pub settings: PathBuf,
    pub spool: PathBuf,
    pub failed: PathBuf,
}

/// Both clocks, read once per event so a handler sees one consistent moment.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub mono: Instant,
    pub unix_ms: i64,
}

impl Now {
    pub fn real() -> Now {
        Now {
            mono: Instant::now(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        }
    }
}

/// Everything the core asks of the outside world.
pub enum Effect {
    StartLive {
        session: SessionId,
        params: live::Params,
        queue: Arc<LiveQueue>,
    },
    StartBatch {
        job: JobId,
        work: batch::Job,
    },
    StartKeyTest {
        job: JobId,
        key: Arc<Secret>,
        cancel: Arc<AtomicBool>,
    },
    Paste(PasteRequest),
    Copy {
        request: RequestId,
        text: String,
    },
    Present(OwnedPresentation),
    RecordingTray(bool),
    /// true = the start chime, false = the stop chime.
    Sound(bool),
    HistoryChanged,
    UiResult(ActionResult),
    SetChord(Chord),
}

pub trait Runtime {
    fn capture_busy(&self) -> bool;
    fn start_capture(&mut self, id: SessionId, wav: &Path) -> io::Result<Capture>;
    fn paste_target(&self) -> Option<PasteTarget>;
    fn store_key(&mut self, key: &Secret) -> io::Result<()>;
    /// `Err` means the effect could not even be handed over (no worker thread).
    fn emit(&mut self, effect: Effect) -> io::Result<()>;
}

pub struct NativeRuntime {
    events: Sender<Event>,
    paste: Option<Sender<paste::Job>>,
}

impl NativeRuntime {
    fn new(events: Sender<Event>) -> NativeRuntime {
        let paste = match paste::spawn(events.clone()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                log::error!("paste worker: {e}");
                None
            }
        };
        NativeRuntime { events, paste }
    }

    fn paste_tx(&self) -> io::Result<&Sender<paste::Job>> {
        self.paste
            .as_ref()
            .ok_or_else(|| io::Error::other("no paste worker"))
    }
}

impl Runtime for NativeRuntime {
    fn store_key(&mut self, key: &Secret) -> io::Result<()> {
        crate::key::store(key.as_str().unwrap_or("")).map_err(io::Error::other)
    }
    fn capture_busy(&self) -> bool {
        capture::busy()
    }

    fn start_capture(&mut self, id: SessionId, wav: &Path) -> io::Result<Capture> {
        capture::start(id, wav.to_path_buf(), self.events.clone())
    }

    fn paste_target(&self) -> Option<PasteTarget> {
        use crate::win::window;
        window::foreground()
            .filter(|&w| !window::is_own(w))
            .map(PasteTarget::capture)
    }

    fn emit(&mut self, effect: Effect) -> io::Result<()> {
        match effect {
            Effect::StartLive {
                session,
                params,
                queue,
            } => live::spawn(session, params, queue, self.events.clone()),
            Effect::StartBatch { job, work } => batch::spawn(job, work, self.events.clone()),
            Effect::StartKeyTest { job, key, cancel } => {
                batch::spawn_key_test(job, key, cancel, self.events.clone())
            }
            Effect::Paste(req) => self
                .paste_tx()?
                .send(paste::Job::Paste(req))
                .map_err(io::Error::other),
            Effect::Copy { request, text } => self
                .paste_tx()?
                .send(paste::Job::Copy { request, text })
                .map_err(io::Error::other),
            Effect::Present(p) => {
                crate::win::overlay::present(p);
                Ok(())
            }
            Effect::RecordingTray(on) => {
                crate::win::tray::set_recording(on);
                Ok(())
            }
            Effect::Sound(start) => {
                if start {
                    crate::sound::start();
                } else {
                    crate::sound::stop();
                }
                Ok(())
            }
            Effect::HistoryChanged => {
                crate::win::app::changed();
                Ok(())
            }
            Effect::UiResult(r) => {
                crate::win::app::reply(r);
                Ok(())
            }
            Effect::SetChord(c) => {
                crate::win::hook::set_chord(c);
                Ok(())
            }
        }
    }
}

/// Everything one dictation needs that must stay the same from start to commit: the settings
/// and key it began with, and the tokens that cancel it or withdraw its right to paste.
struct Dictation {
    id: SessionId,
    created_ms: i64,
    spool: PathBuf,
    key: Arc<Secret>,
    language: Option<String>,
    words: Vec<String>,
    /// Shared by the Live stream and the fallback batch request.
    cancel: Arc<AtomicBool>,
    delivery: Arc<DeliveryPermission>,
}

enum CaptureStatus {
    Waiting,
    Terminal(CaptureReport),
}

enum ProviderStage {
    WaitingLive,
    /// Live's verdict, held until the capture is terminal (the WAV decides whether batch can run).
    Live(TranscriptOutcome),
    WaitingBatchSlot {
        partial: String,
        reason: Failure,
        until: Instant,
    },
    RunningBatch {
        job: JobId,
        partial: String,
    },
    Ready(TranscriptOutcome),
}

/// A stopped dictation waiting for capture to finalize and for Live (then maybe batch).
struct Finish {
    ctx: Dictation,
    target: Option<PasteTarget>,
    capture: CaptureStatus,
    provider: ProviderStage,
    /// Give up waiting after this; the audio goes to history for Retry.
    give_up: Instant,
    /// Set by a press while transcribing; a second press before it expires cancels.
    cancel_armed: Option<Instant>,
    disposition: FinishDisposition,
}

enum State {
    Idle,
    Starting {
        ctx: Dictation,
        cap: Capture,
        deadline: Instant,
    },
    Recording {
        ctx: Dictation,
        cap: Capture,
        opened: Instant,
        /// Live ended on its own while still recording; decided once the user stops.
        early_live: Option<TranscriptOutcome>,
    },
    Finishing(Finish),
}

enum Purpose {
    Fallback {
        session: SessionId,
    },
    Retry {
        row: i64,
        audio: PathBuf,
    },
    KeyTest {
        request: Option<RequestId>,
        generation: u64,
    },
}

struct BatchEntry {
    purpose: Purpose,
    cancel: Arc<AtomicBool>,
}

enum Step {
    Wait,
    Commit,
}

pub struct Core<R: Runtime = NativeRuntime> {
    rt: R,
    paths: Paths,
    dirs: Dirs,
    store: Store,
    settings: Settings,
    key: Option<Arc<Secret>>,
    key_generation: u64,
    gate: Arc<DeliveryGate>,
    state: State,
    /// An abandoned dictation whose capture hasn't reported yet. At most one.
    retiring: Option<Finish>,
    last_session: u64,
    /// The transcript so far for the active dictation.
    partial: String,
    /// The session whose Live worker is still running (at most one).
    live_slot: Option<SessionId>,
    jobs: HashMap<JobId, BatchEntry>,
    by_row: HashMap<i64, JobId>,
    next_job: u64,
    /// Rows saved without text while a worker for that session was still running.
    late_rows: HashMap<SessionId, i64>,
    /// Cancelled starts whose capture may still leave a (worthless) WAV behind.
    discard: HashSet<SessionId>,
    /// Async UI requests awaiting their one reply: request → window generation.
    pending_ui: HashMap<RequestId, u64>,
    hud_epoch: u64,
    hud_owner: Option<HudOwner>,
    paste_pending: Option<SessionId>,
    tray_on: bool,
    quit_by: Option<Instant>,
    now: Now,
}

fn fail(msg: impl Into<String>) -> ActionFailure {
    ActionFailure(msg.into())
}

/// The best result known when a dictation has to end without a provider verdict.
fn best_known(partial: &str, reason: Failure) -> TranscriptOutcome {
    let partial = partial.trim();
    if partial.is_empty() {
        TranscriptOutcome::Failed { reason }
    } else {
        TranscriptOutcome::Incomplete {
            text: partial.to_string(),
            model: LIVE_MODEL,
            reason,
        }
    }
}

/// What batch should be tried for after Live ended this way: (text so far, reason).
fn fallback_of(o: &TranscriptOutcome) -> Option<(String, Failure)> {
    match o {
        TranscriptOutcome::Failed { reason }
            if reason.wants_fallback() || *reason == Failure::Unconfirmed =>
        {
            Some((String::new(), reason.clone()))
        }
        TranscriptOutcome::Incomplete { text, reason, .. } if reason.wants_fallback() => {
            Some((text.clone(), reason.clone()))
        }
        _ => None,
    }
}

fn batch_outcome(result: Result<BatchText, GeminiError>, partial: &str) -> TranscriptOutcome {
    match result {
        Ok(BatchText::Text(text)) => TranscriptOutcome::Complete {
            text,
            model: BATCH_MODEL,
        },
        Ok(BatchText::Empty) if !partial.trim().is_empty() => {
            best_known(partial, Failure::Unconfirmed)
        }
        Ok(BatchText::Empty) => TranscriptOutcome::Empty { model: BATCH_MODEL },
        Err(e) => best_known(partial, Failure::Provider(e)),
    }
}

impl Core<NativeRuntime> {
    pub fn new(paths: Paths, store: Store, events: Sender<Event>) -> Core<NativeRuntime> {
        let settings = Settings::load(&paths.settings);
        let key = crate::key::load().map(Arc::new);
        log::info!(
            "core: key {}",
            if key.is_some() { "present" } else { "missing" }
        );
        let mut core = Core::with_runtime(NativeRuntime::new(events), paths, store, settings, key);
        core.startup();
        core
    }
}

impl<R: Runtime> Core<R> {
    /// Builds a core without touching the machine: no settings read, no recovery, no import.
    pub fn with_runtime(
        rt: R,
        paths: Paths,
        store: Store,
        settings: Settings,
        key: Option<Arc<Secret>>,
    ) -> Core<R> {
        let dirs = Dirs {
            spool: paths.spool.clone(),
            failed: paths.failed.clone(),
        };
        let now = Now::real();
        Core {
            rt,
            paths,
            dirs,
            store,
            settings,
            key,
            key_generation: 0,
            gate: DeliveryGate::new(),
            state: State::Idle,
            retiring: None,
            last_session: 0,
            partial: String::new(),
            live_slot: None,
            jobs: HashMap::new(),
            by_row: HashMap::new(),
            next_job: 1,
            late_rows: HashMap::new(),
            discard: HashSet::new(),
            pending_ui: HashMap::new(),
            hud_epoch: 0,
            hud_owner: None,
            paste_pending: None,
            tray_on: false,
            quit_by: None,
            now,
        }
    }

    /// Real-machine start-up work: chord, folders, crash recovery, first-run import, pruning.
    fn startup(&mut self) {
        let _ = self.rt.emit(Effect::SetChord(self.settings.chord()));
        let _ = std::fs::create_dir_all(&self.paths.spool);
        let _ = std::fs::create_dir_all(&self.paths.failed);
        persist::recover(&self.store, &RealFs, &self.dirs);
        self.last_session = persist::highest_known_session(&self.store, &self.dirs);
        self.enforce_retention();
        if self
            .store
            .meta("openwhispr_import_ms")
            .ok()
            .flatten()
            .is_none()
            && crate::import::openwhispr_dir().is_some()
        {
            let _ = self.import_openwhispr();
        }
        self.prune_history();
    }

    pub fn run(mut self, rx: Receiver<Event>) {
        loop {
            let ev = match self.next_deadline() {
                Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                    Ok(ev) => Some(ev),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => return,
                },
            };
            match ev {
                Some(ev) => self.handle_at(ev, Now::real()),
                None => self.on_deadline_at(Now::real()),
            }
            if self.quit_by.is_some()
                && matches!(self.state, State::Idle)
                && self.retiring.is_none()
            {
                return;
            }
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let state = match &self.state {
            State::Starting { deadline, .. } => Some(*deadline),
            State::Recording { opened, .. } => Some(*opened + MAX_RECORDING),
            State::Finishing(f) => Some(match &f.provider {
                ProviderStage::WaitingBatchSlot { until, .. } => (*until).min(f.give_up),
                _ => f.give_up,
            }),
            State::Idle => None,
        };
        [
            state,
            self.retiring.as_ref().map(|f| f.give_up),
            self.quit_by,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    // ---------------------------------------------------------------- effects and HUD

    fn fx(&mut self, e: Effect) {
        if let Err(err) = self.rt.emit(e) {
            log::warn!("core: effect not delivered: {err}");
        }
    }

    fn claim(&mut self, owner: HudOwner) {
        self.hud_epoch += 1;
        self.hud_owner = Some(owner);
    }

    /// Draws only while `owner` still owns the HUD; a late request from a finished owner is dropped.
    fn present(&mut self, owner: HudOwner, command: HudCommand) {
        if self.hud_owner != Some(owner) {
            return;
        }
        let epoch = self.hud_epoch;
        self.fx(Effect::Present(OwnedPresentation {
            epoch,
            owner,
            command,
        }));
    }

    fn show(&mut self, owner: HudOwner, text: &str, tone: Tone, hide_after: Option<Duration>) {
        self.present(
            owner,
            HudCommand::Show {
                text: text.into(),
                tone,
                hide_after,
                clear: false,
            },
        );
    }

    /// A message with no owner of its own. Only shown while nothing else is using the HUD.
    fn notice(&mut self, text: &str, tone: Tone) {
        if !matches!(self.state, State::Idle) || self.paste_pending.is_some() {
            log::info!("notice held back: {text}");
            return;
        }
        let owner = HudOwner::Notice(RequestId::next());
        self.claim(owner);
        self.present(
            owner,
            HudCommand::Show {
                text: text.into(),
                tone,
                hide_after: Some(NOTICE),
                clear: true,
            },
        );
    }

    fn sync_tray(&mut self) {
        let on = matches!(self.state, State::Recording { .. });
        if on != self.tray_on {
            self.tray_on = on;
            self.fx(Effect::RecordingTray(on));
        }
    }

    fn history_changed(&mut self) {
        self.fx(Effect::HistoryChanged);
    }

    // ---------------------------------------------------------------- events

    pub fn handle_at(&mut self, ev: Event, now: Now) {
        self.now = now;
        match ev {
            Event::Toggle => {
                if self.quit_by.is_none() {
                    self.toggle();
                }
            }
            Event::Power(p) => {
                log::info!("power: {p:?}");
                match p {
                    PowerEvent::Suspend | PowerEvent::Lock => {
                        self.gate.block();
                        self.interrupt(Failure::Interrupted);
                    }
                    PowerEvent::Resume | PowerEvent::Unlock => self.gate.unblock(),
                }
            }
            Event::Ui(req) => self.ui(req),
            Event::Capture { session, ev } => match ev {
                CaptureEvent::Opened => self.opened(session),
                CaptureEvent::Finished(report) => self.capture_finished(session, report),
            },
            Event::LiveText {
                session,
                finals,
                interim,
            } => self.live_text(session, finals, interim),
            Event::Live { session, outcome } => self.live_done(session, outcome),
            Event::Batch { job, result } => self.batch_done(job, result),
            Event::Pasted(p) => self.pasted(p),
            Event::Copied { request, result } => {
                if let Some(g) = self.pending_ui.remove(&request) {
                    if let Err(e) = &result {
                        self.notice(&format!("Couldn't copy — {}", e.0), Tone::Error);
                    }
                    if g == 0 && result.is_ok() {
                        self.notice("Copied last transcription", Tone::Info);
                    }
                    self.reply(request, g, result.map(|()| ActionSuccess::Copied));
                }
            }
            Event::KeyTest { job, result } => self.key_tested(job, result),
            Event::Quit => self.quit(),
        }
        self.sync_tray();
    }

    pub fn on_deadline_at(&mut self, now: Now) {
        self.now = now;
        let t = now.mono;
        if self.quit_by.is_some_and(|q| t >= q) {
            log::warn!("core: quitting before the dictation finished");
            self.state = State::Idle;
            self.retiring = None;
            self.sync_tray();
            return;
        }
        if self.retiring.as_ref().is_some_and(|f| t >= f.give_up) {
            // The capture never reported. Its WAV stays; startup recovery turns it into a row.
            log::warn!("core: abandoned capture never reported");
            self.retiring = None;
        }
        match &self.state {
            State::Starting { deadline, .. } if t >= *deadline => {
                log::warn!("core: microphone didn't open in time");
                self.cancel_start();
                self.notice("Microphone didn't respond", Tone::Error);
            }
            State::Recording { opened, .. } if t >= *opened + MAX_RECORDING => {
                log::info!("core: recording limit reached");
                self.finish_recording(FinishDisposition::Deliver, CaptureStatus::Waiting);
            }
            State::Finishing(f) if t >= f.give_up => {
                log::warn!("core: transcription timed out");
                self.abandon(Failure::Timeout);
            }
            State::Finishing(_) => self.advance(),
            _ => {}
        }
        self.sync_tray();
    }

    fn quit(&mut self) {
        self.gate.block();
        if self.quit_by.is_none() {
            self.quit_by = Some(self.now.mono + QUIT_WAIT);
        }
        self.interrupt(Failure::Interrupted);
    }

    /// Lock, sleep or quit: nothing may be delivered any more, and nothing is lost.
    fn interrupt(&mut self, why: Failure) {
        if matches!(self.state, State::Starting { .. }) {
            self.cancel_start();
        } else if matches!(self.state, State::Recording { .. }) {
            self.finish_recording(FinishDisposition::SaveOnly(why), CaptureStatus::Waiting);
        } else if let State::Finishing(f) = &mut self.state {
            f.ctx.delivery.revoke();
            if f.disposition == FinishDisposition::Deliver {
                f.disposition = FinishDisposition::SaveOnly(why);
            }
        }
    }

    fn active_id(&self) -> Option<SessionId> {
        match &self.state {
            State::Starting { ctx, .. } | State::Recording { ctx, .. } => Some(ctx.id),
            State::Finishing(f) => Some(f.ctx.id),
            State::Idle => None,
        }
    }

    fn next_session(&mut self) -> Option<SessionId> {
        let wanted = u64::try_from(self.now.unix_ms).unwrap_or(0);
        let next = wanted.max(self.last_session.checked_add(1)?);
        if next > i64::MAX as u64 {
            return None;
        }
        self.last_session = next;
        Some(SessionId(next))
    }

    // ---------------------------------------------------------------- dictation

    fn toggle(&mut self) {
        match &self.state {
            State::Idle => self.start(),
            State::Starting { .. } => {
                self.cancel_start();
            }
            State::Recording { .. } => {
                self.finish_recording(FinishDisposition::Deliver, CaptureStatus::Waiting)
            }
            State::Finishing(f) => {
                let armed = f
                    .cancel_armed
                    .is_some_and(|t| self.now.mono.saturating_duration_since(t) < CANCEL_WINDOW);
                if armed {
                    log::info!("core: transcription cancelled");
                    self.abandon(Failure::Cancelled);
                } else if let State::Finishing(f) = &mut self.state {
                    f.cancel_armed = Some(self.now.mono);
                    let id = f.ctx.id;
                    self.show(
                        HudOwner::Dictation(id),
                        "Transcribing — press again to cancel",
                        Tone::Busy,
                        None,
                    );
                }
            }
        }
    }

    fn start(&mut self) {
        let Some(key) = self.key.clone() else {
            self.notice(&GeminiError::KeyMissing.to_string(), Tone::Error);
            return;
        };
        if self.retiring.is_some() {
            self.notice("Still saving the last recording", Tone::Info);
            return;
        }
        if self.rt.capture_busy() {
            self.notice("Microphone still stuck — replug it", Tone::Error);
            return;
        }
        let Some(id) = self.next_session() else {
            self.notice("Couldn't start a new recording", Tone::Error);
            return;
        };
        let spool = self.dirs.spool_path(id);
        let ctx = Dictation {
            id,
            created_ms: self.now.unix_ms,
            spool: spool.clone(),
            key,
            language: self.settings.language().map(str::to_string),
            words: self.store.dictionary().unwrap_or_default(),
            cancel: Arc::new(AtomicBool::new(false)),
            delivery: self.gate.permit(),
        };
        let cap = match self.rt.start_capture(id, &spool) {
            Ok(c) => c,
            Err(e) => {
                log::error!("core: capture did not start: {e}");
                self.notice("Couldn't start the microphone", Tone::Error);
                return;
            }
        };
        self.partial.clear();
        self.paste_pending = None;
        self.state = State::Starting {
            ctx,
            cap,
            deadline: self.now.mono + OPEN_TIMEOUT,
        };
        if self.settings.sounds {
            self.fx(Effect::Sound(true));
        }
        let owner = HudOwner::Dictation(id);
        self.claim(owner);
        self.show(owner, "Starting…", Tone::Busy, None);
    }

    /// Starting → Idle with nothing said. Whatever WAV the capture may still write is worthless.
    fn cancel_start(&mut self) {
        let State::Starting { ctx, cap, .. } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return;
        };
        cap.stop();
        ctx.cancel.store(true, Ordering::Release);
        self.discard.insert(ctx.id);
        let owner = HudOwner::Dictation(ctx.id);
        self.present(owner, HudCommand::Hide);
    }

    fn opened(&mut self, session: SessionId) {
        if self.active_id() != Some(session) || !matches!(self.state, State::Starting { .. }) {
            return;
        }
        let State::Starting { ctx, cap, .. } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return;
        };
        let mut early_live = None;
        if self.live_slot.is_some() {
            early_live = Some(TranscriptOutcome::Failed {
                reason: Failure::Internal("a previous Live stream is still closing".into()),
            });
        } else {
            let params = live::Params {
                key: ctx.key.clone(),
                language: ctx.language.clone(),
                words: ctx.words.clone(),
                cancel: ctx.cancel.clone(),
            };
            let effect = Effect::StartLive {
                session,
                params,
                queue: cap.queue.clone(),
            };
            match self.rt.emit(effect) {
                Ok(()) => self.live_slot = Some(session),
                Err(e) => {
                    log::error!("core: Live did not start: {e}");
                    early_live = Some(TranscriptOutcome::Failed {
                        reason: Failure::Internal("couldn't start the Live worker".into()),
                    });
                }
            }
        }
        let owner = HudOwner::Dictation(session);
        self.state = State::Recording {
            ctx,
            cap,
            opened: self.now.mono,
            early_live,
        };
        self.show(owner, "Listening…", Tone::Recording, None);
        self.present(owner, HudCommand::Limit(MAX_RECORDING));
    }

    fn finish_recording(&mut self, disposition: FinishDisposition, capture: CaptureStatus) {
        if !matches!(self.state, State::Recording { .. }) {
            return;
        }
        let State::Recording {
            ctx,
            cap,
            opened,
            early_live,
        } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return;
        };
        cap.stop();
        drop(cap);
        if self.settings.sounds {
            self.fx(Effect::Sound(false));
        }
        log::info!(
            "core {}: stop after {} ms",
            ctx.id.0,
            self.now.mono.saturating_duration_since(opened).as_millis()
        );
        let owner = HudOwner::Dictation(ctx.id);
        let f = Finish {
            target: self.rt.paste_target(),
            capture,
            provider: early_live.map_or(ProviderStage::WaitingLive, ProviderStage::Live),
            give_up: self.now.mono + FINISH_WAIT,
            cancel_armed: None,
            disposition,
            ctx,
        };
        self.show(owner, "Transcribing…", Tone::Busy, None);
        self.state = State::Finishing(f);
        self.advance();
    }

    fn capture_finished(&mut self, session: SessionId, report: CaptureReport) {
        log::info!(
            "core {}: captured {} ms, dropped {}, audio {:?}, problem {:?}",
            session.0,
            report.duration_ms,
            report.dropped_chunks,
            report.audio,
            report.problem
        );
        if self.active_id() == Some(session) {
            match &self.state {
                State::Starting { .. } => return self.start_ended(report),
                State::Recording { .. } => {
                    // The microphone went away (or the writer failed) under a live recording.
                    return self.finish_recording(
                        FinishDisposition::Deliver,
                        CaptureStatus::Terminal(report),
                    );
                }
                _ => {
                    if let State::Finishing(f) = &mut self.state {
                        f.capture = CaptureStatus::Terminal(report);
                    }
                    return self.advance();
                }
            }
        }
        if self.retiring.as_ref().is_some_and(|f| f.ctx.id == session) {
            if let Some(mut f) = self.retiring.take() {
                f.capture = CaptureStatus::Terminal(report);
                self.commit(f);
            }
            return;
        }
        // Not ours any more (a start that was cancelled, or a late straggler).
        if report.audio != AudioState::Absent {
            if self.discard.remove(&session) {
                let _ = RealFs.remove(&self.dirs.spool_path(session));
            } else {
                log::warn!(
                    "core {}: capture report for a finished session; startup recovery will pick up the audio",
                    session.0
                );
            }
        } else {
            self.discard.remove(&session);
        }
    }

    /// The capture ended on its own before it ever opened.
    fn start_ended(&mut self, report: CaptureReport) {
        let State::Starting { ctx, cap, .. } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return;
        };
        cap.stop();
        let reason = Failure::Capture(
            report
                .problem
                .clone()
                .unwrap_or_else(|| "microphone didn't respond".into()),
        );
        if report.audio == AudioState::Absent {
            self.present(HudOwner::Dictation(ctx.id), HudCommand::Hide);
            self.notice(
                report
                    .problem
                    .as_deref()
                    .unwrap_or("Microphone didn't respond"),
                Tone::Error,
            );
            return;
        }
        let f = Finish {
            target: None,
            capture: CaptureStatus::Terminal(report),
            provider: ProviderStage::Ready(TranscriptOutcome::Failed { reason }),
            give_up: self.now.mono + FINISH_WAIT,
            cancel_armed: None,
            disposition: FinishDisposition::SaveOnly(Failure::Capture("microphone stopped".into())),
            ctx,
        };
        self.commit(f);
    }

    fn live_text(&mut self, session: SessionId, finals: String, interim: String) {
        if self.active_id() != Some(session)
            || !matches!(self.state, State::Recording { .. } | State::Finishing(_))
        {
            return;
        }
        self.partial = [finals.trim(), interim.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        self.present(
            HudOwner::Dictation(session),
            HudCommand::Words(finals, interim),
        );
    }

    fn live_done(&mut self, session: SessionId, outcome: TranscriptOutcome) {
        if self.live_slot == Some(session) {
            self.live_slot = None;
        }
        if self.active_id() == Some(session) {
            match &mut self.state {
                State::Recording { early_live, .. } => {
                    // Live ended on its own (socket dropped, stream limit): keep recording; the
                    // result is decided once the user stops.
                    *early_live = Some(outcome);
                    return;
                }
                State::Finishing(f) if matches!(f.provider, ProviderStage::WaitingLive) => {
                    f.provider = ProviderStage::Live(outcome);
                    return self.advance();
                }
                _ => return,
            }
        }
        self.settle_late(session, Some(&outcome));
    }

    /// A worker result for a session that was already committed (empty): fill the row in.
    fn settle_late(&mut self, session: SessionId, outcome: Option<&TranscriptOutcome>) {
        if let (Some(&row), Some(o)) = (self.late_rows.get(&session), outcome) {
            let late = match o {
                TranscriptOutcome::Complete { text, model } => Some((text.as_str(), *model)),
                TranscriptOutcome::Incomplete { text, model, .. } if !text.trim().is_empty() => {
                    Some((text.trim(), *model))
                }
                _ => None,
            };
            if let Some((text, model)) = late {
                match self.store.apply_late_text(
                    row,
                    text,
                    model,
                    Some("Arrived after the dictation was saved"),
                ) {
                    Ok(true) => self.history_changed(),
                    Ok(false) => {}
                    Err(e) => log::error!("late text for row {row}: {e}"),
                }
            }
        }
        if !self.worker_outstanding(session) {
            self.late_rows.remove(&session);
        }
    }

    fn worker_outstanding(&self, session: SessionId) -> bool {
        self.live_slot == Some(session)
            || self
                .jobs
                .values()
                .any(|e| matches!(e.purpose, Purpose::Fallback { session: s } if s == session))
    }

    // ---------------------------------------------------------------- finishing

    /// Moves a finishing dictation forward as far as it can go.
    fn advance(&mut self) {
        let State::Finishing(mut f) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        match self.step(&mut f) {
            Step::Wait => self.state = State::Finishing(f),
            Step::Commit => self.commit(f),
        }
    }

    fn step(&mut self, f: &mut Finish) -> Step {
        loop {
            let audio = match &f.capture {
                CaptureStatus::Terminal(r) => Some(r.audio),
                CaptureStatus::Waiting => None,
            };
            match &f.provider {
                ProviderStage::Live(o) if audio.is_some() => {
                    let o = o.clone();
                    f.provider = match fallback_of(&o) {
                        Some((partial, reason))
                            if f.disposition == FinishDisposition::Deliver
                                && audio == Some(AudioState::Finalized) =>
                        {
                            ProviderStage::WaitingBatchSlot {
                                partial,
                                reason,
                                until: self.now.mono + BATCH_SLOT_WAIT,
                            }
                        }
                        _ => ProviderStage::Ready(o),
                    };
                }
                ProviderStage::WaitingBatchSlot { .. } => {
                    let ProviderStage::WaitingBatchSlot {
                        partial,
                        reason,
                        until,
                    } = std::mem::replace(&mut f.provider, ProviderStage::WaitingLive)
                    else {
                        return Step::Wait;
                    };
                    if self.now.mono >= until {
                        f.provider = ProviderStage::Ready(best_known(&partial, reason));
                        continue;
                    }
                    let Some(job) = self.alloc_job(
                        Purpose::Fallback { session: f.ctx.id },
                        f.ctx.cancel.clone(),
                    ) else {
                        f.provider = ProviderStage::WaitingBatchSlot {
                            partial,
                            reason,
                            until,
                        };
                        return Step::Wait;
                    };
                    log::info!("core {}: Live ended ({reason}); batch fallback", f.ctx.id.0);
                    let work = batch::Job {
                        key: f.ctx.key.clone(),
                        language: f.ctx.language.clone(),
                        words: f.ctx.words.clone(),
                        wav: f.ctx.spool.clone(),
                        cancel: f.ctx.cancel.clone(),
                    };
                    let len = RealFs.len(&f.ctx.spool);
                    match self.rt.emit(Effect::StartBatch { job, work }) {
                        Ok(()) => {
                            f.give_up = self.now.mono + batch::max_wait(len) + BATCH_SLACK;
                            f.provider = ProviderStage::RunningBatch { job, partial };
                            return Step::Wait;
                        }
                        Err(e) => {
                            log::error!("core: batch did not start: {e}");
                            self.take_job(job);
                            f.provider = ProviderStage::Ready(best_known(
                                &partial,
                                Failure::Internal("couldn't start the batch worker".into()),
                            ));
                        }
                    }
                }
                ProviderStage::Ready(_) if audio.is_some() => return Step::Commit,
                _ => return Step::Wait,
            }
        }
    }

    /// Ends a finishing dictation without waiting for the provider any more. Text we already
    /// have is kept (provisionally); the audio is kept for Retry; nothing is pasted.
    fn abandon(&mut self, why: Failure) {
        let State::Finishing(mut f) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        if f.disposition == FinishDisposition::Deliver {
            f.disposition = FinishDisposition::SaveOnly(why.clone());
        }
        f.ctx.delivery.revoke();
        f.ctx.cancel.store(true, Ordering::Release);
        let outcome = match std::mem::replace(&mut f.provider, ProviderStage::WaitingLive) {
            ProviderStage::Ready(o) | ProviderStage::Live(o) => o,
            ProviderStage::WaitingBatchSlot {
                partial, reason, ..
            } => best_known(&partial, reason),
            ProviderStage::RunningBatch { partial, .. } => best_known(&partial, why),
            ProviderStage::WaitingLive => best_known(&self.partial, why),
        };
        f.provider = ProviderStage::Ready(outcome);
        if matches!(f.capture, CaptureStatus::Terminal(_)) {
            self.commit(f);
        } else {
            // Never rename a WAV the capture still has open.
            f.give_up = self.now.mono + FINISH_WAIT;
            self.present(
                HudOwner::Dictation(f.ctx.id),
                HudCommand::Show {
                    text: "Saving…".into(),
                    tone: Tone::Busy,
                    hide_after: None,
                    clear: false,
                },
            );
            self.retiring = Some(f);
        }
    }

    // ---------------------------------------------------------------- commit

    fn commit(&mut self, f: Finish) {
        let CaptureStatus::Terminal(report) = &f.capture else {
            log::error!("core: commit before the capture ended");
            return;
        };
        let session = f.ctx.id;
        let outcome = match &f.provider {
            ProviderStage::Ready(o) => o.clone(),
            _ => best_known(&self.partial, Failure::Internal("no result".into())),
        };
        let decision = persist::decide_commit(&outcome, report, &f.disposition);
        let commit = Commit {
            session,
            created_ms: f.ctx.created_ms,
            duration_ms: report.duration_ms,
            decision: &decision,
            audio: report.audio,
        };
        let persisted = match persist::persist_dictation(&self.store, &RealFs, &self.dirs, &commit)
        {
            Ok(p) => p,
            Err(e) => {
                // The WAV is untouched; startup recovery turns it into a row.
                log::error!("core {}: history insert failed: {e}", session.0);
                self.notice("Couldn't save to history", Tone::Error);
                return;
            }
        };
        if self.active_id().is_none() {
            self.partial.clear();
        }
        if !persisted.fresh {
            log::warn!("core {}: already stored", session.0);
            return;
        }
        let Some(row) = persisted.row_id else {
            self.present(HudOwner::Dictation(session), HudCommand::Hide);
            self.notice("Nothing heard", Tone::Info);
            return;
        };
        log::info!(
            "core {}: row {row} {}, {} chars",
            session.0,
            decision.status,
            decision.text.len()
        );
        if decision.text.is_empty() && self.worker_outstanding(session) {
            self.late_rows.insert(session, row);
        }
        self.enforce_retention();
        self.prune_history();
        self.history_changed();

        if decision.text.is_empty() {
            let msg = decision
                .error
                .clone()
                .unwrap_or_else(|| "Transcription failed".into());
            let text = if persisted.audio.is_some() {
                format!("{msg} — audio saved in history")
            } else {
                msg
            };
            self.present(HudOwner::Dictation(session), HudCommand::Hide);
            self.notice(&text, Tone::Error);
            return;
        }
        if !decision.may_deliver || !f.ctx.delivery.allowed() {
            self.present(HudOwner::Dictation(session), HudCommand::Hide);
            self.notice("Saved to history — not pasted", Tone::Info);
            return;
        }
        let req = PasteRequest {
            session,
            row_id: row,
            text: decision.text.clone(),
            target: f.target,
            incomplete: decision.incomplete_paste,
            permission: f.ctx.delivery.clone(),
        };
        self.paste_pending = Some(session);
        if let Err(e) = self.rt.emit(Effect::Paste(req)) {
            log::error!("core {}: paste not handed over: {e}", session.0);
            self.paste_pending = None;
            let _ = self.store.set_paste(row, "failed:worker");
            self.present(HudOwner::Dictation(session), HudCommand::Hide);
            self.notice("Couldn't paste — text is in history", Tone::Error);
        }
    }

    fn pasted(&mut self, p: paste::Pasted) {
        if let Err(e) = self.store.set_paste(p.row_id, &p.outcome.db_value()) {
            log::error!("store paste: {e}");
        }
        if self.paste_pending == Some(p.session) {
            self.paste_pending = None;
        }
        let owner = HudOwner::Dictation(p.session);
        match p.outcome.notice(p.incomplete) {
            // Keep the words on screen so the user can see what didn't land.
            Some((n, tone)) => self.show(owner, n, tone, Some(NOTICE)),
            None => self.present(owner, HudCommand::Done),
        }
        self.history_changed();
    }

    // ---------------------------------------------------------------- batch jobs

    fn alloc_job(&mut self, purpose: Purpose, cancel: Arc<AtomicBool>) -> Option<JobId> {
        if self.jobs.len() >= MAX_BATCH_WORKERS {
            return None;
        }
        let id = JobId(self.next_job);
        self.next_job += 1;
        self.jobs.insert(id, BatchEntry { purpose, cancel });
        Some(id)
    }

    /// Frees a batch slot exactly once, whichever way the job ended.
    fn take_job(&mut self, job: JobId) -> Option<BatchEntry> {
        let entry = self.jobs.remove(&job)?;
        if let Purpose::Retry { row, .. } = &entry.purpose
            && self.by_row.get(row) == Some(&job)
        {
            self.by_row.remove(row);
        }
        Some(entry)
    }

    /// Audio files a running Retry still reads: neither retention nor delete may touch them.
    fn pinned(&self) -> HashSet<PathBuf> {
        self.jobs
            .values()
            .filter_map(|e| match &e.purpose {
                Purpose::Retry { audio, .. } => Some(audio.clone()),
                _ => None,
            })
            .collect()
    }

    fn sweep(&mut self) {
        persist::cleanup_tombstones(&self.store, &RealFs, &self.dirs, &self.pinned());
    }

    fn batch_done(&mut self, job: JobId, result: Result<BatchText, GeminiError>) {
        let Some(entry) = self.take_job(job) else {
            return;
        };
        match entry.purpose {
            Purpose::Fallback { session } => {
                if let State::Finishing(f) = &mut self.state
                    && f.ctx.id == session
                    && matches!(&f.provider, ProviderStage::RunningBatch { job: j, .. } if *j == job)
                {
                    let ProviderStage::RunningBatch { partial, .. } =
                        std::mem::replace(&mut f.provider, ProviderStage::WaitingLive)
                    else {
                        return;
                    };
                    f.provider = ProviderStage::Ready(batch_outcome(result, &partial));
                    return self.advance();
                }
                let late = batch_outcome(result, "");
                self.settle_late(session, Some(&late));
            }
            Purpose::Retry { row, audio } => {
                self.retry_done(
                    job,
                    row,
                    &audio,
                    result,
                    entry.cancel.load(Ordering::Acquire),
                );
            }
            Purpose::KeyTest { .. } => {}
        }
        self.sweep();
    }

    fn retry_done(
        &mut self,
        job: JobId,
        row: i64,
        audio: &Path,
        result: Result<BatchText, GeminiError>,
        cancelled: bool,
    ) {
        let _ = job;
        // A deleted row or a cancelled retry: the result has nowhere to go.
        if cancelled || !matches!(self.store.get(row), Ok(Some(_))) {
            return;
        }
        match result {
            Ok(BatchText::Text(t)) => {
                match self.store.apply_retry(row, audio, t.as_str(), BATCH_MODEL) {
                    Ok(true) => {
                        self.history_changed();
                        self.notice("Retried — saved to history", Tone::Info);
                    }
                    Ok(false) => log::warn!("retry {row}: the row's audio changed meanwhile"),
                    Err(e) => log::error!("retry {row}: {e}"),
                }
            }
            Ok(BatchText::Empty) => {
                let _ = self.store.set_retry_error(row, audio, "No speech detected");
                self.history_changed();
                self.notice("No speech detected", Tone::Info);
            }
            Err(e) => {
                let _ = self.store.set_retry_error(row, audio, &e.to_string());
                self.history_changed();
                self.notice(&e.to_string(), Tone::Error);
            }
        }
    }

    fn key_tested(&mut self, job: JobId, result: Result<(), GeminiError>) {
        let Some(entry) = self.take_job(job) else {
            return;
        };
        let Purpose::KeyTest {
            request,
            generation,
        } = entry.purpose
        else {
            return;
        };
        let stale = generation != self.key_generation;
        let reply = if stale {
            Err(fail("The key changed during the check"))
        } else {
            match &result {
                Ok(()) => {
                    self.notice("Gemini key works", Tone::Info);
                    Ok(ActionSuccess::KeyChecked)
                }
                Err(e) => {
                    self.notice(&e.to_string(), Tone::Error);
                    Err(fail(e.to_string()))
                }
            }
        };
        if let Some(req) = request
            && let Some(g) = self.pending_ui.remove(&req)
        {
            self.reply(req, g, reply);
        }
    }

    // ---------------------------------------------------------------- UI actions

    fn reply(
        &mut self,
        id: RequestId,
        window_generation: u64,
        result: Result<ActionSuccess, ActionFailure>,
    ) {
        self.fx(Effect::UiResult(ActionResult {
            id,
            window_generation,
            result,
        }));
    }

    /// Every request gets exactly one reply: now, or (Copy, TestKey) when the work finishes.
    fn ui(&mut self, req: ActionRequest) {
        let ActionRequest {
            id,
            window_generation: g,
            action,
        } = req;
        if matches!(action, Action::CopyLatest) {
            match self.store.latest_text() {
                Ok(Some(row)) => self.ui(ActionRequest {
                    id,
                    window_generation: g,
                    action: Action::Copy(row.id),
                }),
                _ => {
                    self.notice("No transcription to copy yet", Tone::Info);
                    self.reply(id, g, Err(fail("No transcription to copy yet")));
                }
            }
            return;
        }
        // The History window has no place for these failures; the HUD says them.
        let notify = matches!(
            action,
            Action::Retry(_) | Action::Delete(_) | Action::ClearBefore(_)
        );
        let result = match action {
            Action::CopyLatest => unreachable!("handled above"),
            Action::Copy(row) => match self.store.get(row) {
                Ok(Some(r)) if !r.text.is_empty() => {
                    if self.pending_ui.len() >= 16 {
                        self.reply(id, g, Err(fail("Another copy is pending")));
                        return;
                    }
                    match self.rt.emit(Effect::Copy {
                        request: id,
                        text: r.text,
                    }) {
                        Ok(()) => {
                            self.pending_ui.insert(id, g);
                            return;
                        }
                        Err(_) => Err(fail("Couldn't copy to the clipboard")),
                    }
                }
                _ => Err(fail("Nothing to copy")),
            },
            Action::Retry(row) => self.retry(row).map(|()| ActionSuccess::Done),
            Action::Delete(row) => self.delete(row).map(|()| ActionSuccess::Done),
            Action::ClearBefore(cutoff) => self.clear_before(cutoff).map(|()| ActionSuccess::Done),
            Action::SaveSettings(s) => self.save_settings(s).map(|()| ActionSuccess::SettingsSaved),
            Action::SetDictionary(words) => match self.store.set_dictionary(&words) {
                Ok(()) => Ok(ActionSuccess::DictionarySaved),
                Err(e) => {
                    log::error!("dictionary: {e}");
                    Err(fail("Couldn't save the dictionary"))
                }
            },
            Action::SetApiKey(k) => self.set_key(&k),
            Action::TestKey => match self.start_key_test(Some(id)) {
                Ok(()) => {
                    self.pending_ui.insert(id, g);
                    return;
                }
                Err(e) => Err(e),
            },
            Action::ImportOpenWhispr => self.import_openwhispr().map(|()| ActionSuccess::Done),
            Action::SetAutostart(on) => match crate::autostart::set(on) {
                Ok(()) => Ok(ActionSuccess::Done),
                Err(e) => {
                    log::error!("autostart: {e}");
                    self.notice("Couldn't change start with Windows", Tone::Error);
                    Err(fail("Couldn't change start with Windows"))
                }
            },
        };
        if notify && let Err(e) = &result {
            self.notice(&e.0, Tone::Error);
        }
        self.reply(id, g, result);
    }

    fn retry(&mut self, row: i64) -> Result<(), ActionFailure> {
        let key = self
            .key
            .clone()
            .ok_or_else(|| fail(GeminiError::KeyMissing.to_string()))?;
        if !matches!(self.state, State::Idle) || self.retiring.is_some() {
            return Err(fail("Finish the current dictation first"));
        }
        if self.by_row.contains_key(&row) {
            return Err(fail("Already retrying that one"));
        }
        let audio = match self.store.get(row) {
            Ok(Some(r)) => r.audio_path,
            _ => return Err(fail("That entry no longer exists")),
        };
        let Some(audio) = audio.map(PathBuf::from) else {
            self.notice("No audio kept for that one", Tone::Info);
            return Err(fail("No audio kept for that one"));
        };
        if !self.dirs.owns(&audio) || !RealFs.exists(&audio).unwrap_or(false) {
            return Err(fail("The audio file is missing"));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let purpose = Purpose::Retry {
            row,
            audio: audio.clone(),
        };
        let job = self
            .alloc_job(purpose, cancel.clone())
            .ok_or_else(|| fail("Another transcription is running — try again shortly"))?;
        let work = batch::Job {
            key,
            language: self.settings.language().map(str::to_string),
            words: self.store.dictionary().unwrap_or_default(),
            wav: audio,
            cancel,
        };
        if let Err(e) = self.rt.emit(Effect::StartBatch { job, work }) {
            log::error!("retry: {e}");
            self.take_job(job);
            return Err(fail("Couldn't start the retry"));
        }
        self.by_row.insert(row, job);
        let owner = HudOwner::Retry(job);
        self.claim(owner);
        self.show(owner, "Retrying…", Tone::Busy, None);
        Ok(())
    }

    fn delete(&mut self, row: i64) -> Result<(), ActionFailure> {
        // A Retry still reading this row's audio is cancelled; its file goes when it exits.
        if let Some(job) = self.by_row.remove(&row)
            && let Some(e) = self.jobs.get(&job)
        {
            e.cancel.store(true, Ordering::Release);
        }
        match self.store.delete(row) {
            Ok(_) => {
                self.sweep();
                self.history_changed();
                Ok(())
            }
            Err(e) => {
                log::error!("delete {row}: {e}");
                Err(fail("Couldn't delete that entry"))
            }
        }
    }

    fn clear_before(&mut self, cutoff_ms: i64) -> Result<(), ActionFailure> {
        for (&row, job) in &self.by_row.clone() {
            // Cancel retries of rows that are about to go.
            if self
                .store
                .get(row)
                .ok()
                .flatten()
                .is_some_and(|r| r.created_ms < cutoff_ms)
                && let Some(e) = self.jobs.get(job)
            {
                e.cancel.store(true, Ordering::Release);
                self.by_row.remove(&row);
            }
        }
        match self.store.delete_before(cutoff_ms) {
            Ok(_) => {
                self.sweep();
                self.history_changed();
                Ok(())
            }
            Err(e) => {
                log::error!("clear history: {e}");
                Err(fail("Couldn't clear history"))
            }
        }
    }

    /// Durable first, then live: the running settings change only once the file has them.
    fn save_settings(&mut self, s: Settings) -> Result<(), ActionFailure> {
        if let Err(e) = s.save(&self.paths.settings) {
            log::error!("settings save: {e}");
            return Err(fail("Couldn't save settings"));
        }
        self.fx(Effect::SetChord(s.chord()));
        let prune = s.keep_days != self.settings.keep_days;
        self.settings = s;
        if prune {
            self.prune_history();
        }
        Ok(())
    }

    fn set_key(&mut self, key: &str) -> Result<ActionSuccess, ActionFailure> {
        let key = key.trim();
        if key.is_empty() {
            return Err(fail("Enter a key"));
        }
        let candidate = Arc::new(Secret::new(key.as_bytes().to_vec()));
        if let Err(e) = self.rt.store_key(&candidate) {
            log::error!("key store: {e}");
            return Err(fail("Couldn't save the key"));
        }
        self.key = Some(candidate);
        self.key_generation += 1;
        // The check shares the single batch slot; if it is busy the key is saved but unchecked.
        let test_started = self.start_key_test(None).is_ok();
        Ok(ActionSuccess::KeySaved { test_started })
    }

    fn start_key_test(&mut self, request: Option<RequestId>) -> Result<(), ActionFailure> {
        let key = self
            .key
            .clone()
            .ok_or_else(|| fail(GeminiError::KeyMissing.to_string()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let job = self
            .alloc_job(
                Purpose::KeyTest {
                    request,
                    generation: self.key_generation,
                },
                cancel.clone(),
            )
            .ok_or_else(|| fail("Another transcription is running — try again shortly"))?;
        if let Err(e) = self.rt.emit(Effect::StartKeyTest { job, key, cancel }) {
            log::error!("key test: {e}");
            self.take_job(job);
            return Err(fail("Couldn't start the key check"));
        }
        Ok(())
    }

    fn import_openwhispr(&mut self) -> Result<(), ActionFailure> {
        let Some(dir) = crate::import::openwhispr_dir() else {
            self.notice("OpenWhispr data not found", Tone::Info);
            return Err(fail("OpenWhispr data not found"));
        };
        let history = crate::import::history(&mut self.store, &dir);
        match &history {
            Ok(r) => log::info!("import: {r:?}"),
            Err(e) => log::warn!("import: {e}"),
        }
        let key_note = if self.key.is_some() {
            ""
        } else {
            match crate::import::api_key(&dir) {
                Ok(k) => match crate::key::store(k.as_str().unwrap_or("")) {
                    Ok(()) => {
                        self.key = crate::key::load().map(Arc::new);
                        self.key_generation += 1;
                        log::info!("import: API key imported");
                        ", key imported"
                    }
                    Err(e) => {
                        log::error!("import: key store: {e}");
                        ", key not imported"
                    }
                },
                Err(e) => {
                    log::warn!("import: key: {e}");
                    ", key not imported"
                }
            }
        };
        self.history_changed();
        match history {
            Ok(r) => {
                self.notice(
                    &format!("Imported {} from OpenWhispr{key_note}", r.imported),
                    Tone::Info,
                );
                Ok(())
            }
            Err(e) => {
                self.notice(&e, Tone::Error);
                Err(fail(e))
            }
        }
    }

    // ---------------------------------------------------------------- housekeeping

    /// Applies the "keep history" setting.
    fn prune_history(&mut self) {
        if self.settings.keep_days > 0 {
            let cutoff = self.now.unix_ms - i64::from(self.settings.keep_days) * 86_400_000;
            let _ = self.clear_before(cutoff);
        }
    }

    /// At most `KEEP_FILES` kept WAVs / `KEEP_BYTES`, oldest dropped first.
    fn enforce_retention(&mut self) {
        let r = persist::enforce_retention(
            &self.store,
            &RealFs,
            KEEP_FILES,
            KEEP_BYTES,
            &self.pinned(),
        );
        if r.removed > 0 || r.excess > 0 {
            log::info!("retention: {r:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::NonEmptyText;
    use crate::paste::Outcome;
    use crate::store::{self, NewRow};

    #[derive(Default)]
    struct Rec {
        fx: Vec<Effect>,
        busy: bool,
        fail_capture: bool,
        fail_live: bool,
        fail_batch: bool,
        fail_paste: bool,
        fail_key: bool,
        target: Option<PasteTarget>,
        captures: Vec<(SessionId, Arc<AtomicBool>)>,
    }

    impl Runtime for Rec {
        fn store_key(&mut self, _: &Secret) -> io::Result<()> {
            if self.fail_key {
                Err(io::Error::other("credential unavailable"))
            } else {
                Ok(())
            }
        }
        fn capture_busy(&self) -> bool {
            self.busy
        }
        fn start_capture(&mut self, id: SessionId, _: &Path) -> io::Result<Capture> {
            if self.fail_capture {
                return Err(io::Error::other("no mic"));
            }
            let (cap, stop) = Capture::detached();
            self.captures.push((id, stop));
            Ok(cap)
        }
        fn paste_target(&self) -> Option<PasteTarget> {
            self.target
        }
        fn emit(&mut self, e: Effect) -> io::Result<()> {
            match &e {
                Effect::StartLive { .. } if self.fail_live => return Err(io::Error::other("x")),
                Effect::StartBatch { .. } | Effect::StartKeyTest { .. } if self.fail_batch => {
                    return Err(io::Error::other("x"));
                }
                Effect::Paste(_) | Effect::Copy { .. } if self.fail_paste => {
                    return Err(io::Error::other("x"));
                }
                _ => {}
            }
            self.fx.push(e);
            Ok(())
        }
    }

    impl Rec {
        fn pastes(&self) -> Vec<&PasteRequest> {
            self.fx
                .iter()
                .filter_map(|e| match e {
                    Effect::Paste(p) => Some(p),
                    _ => None,
                })
                .collect()
        }
        fn count(&self, f: impl Fn(&Effect) -> bool) -> usize {
            self.fx.iter().filter(|e| f(e)).count()
        }
        fn presents(&self) -> Vec<&OwnedPresentation> {
            self.fx
                .iter()
                .filter_map(|e| match e {
                    Effect::Present(p) => Some(p),
                    _ => None,
                })
                .collect()
        }
        fn results(&self) -> Vec<&ActionResult> {
            self.fx
                .iter()
                .filter_map(|e| match e {
                    Effect::UiResult(r) => Some(r),
                    _ => None,
                })
                .collect()
        }
        fn last_text(&self) -> Option<String> {
            self.presents().iter().rev().find_map(|p| match &p.command {
                HudCommand::Show { text, .. } => Some(text.clone()),
                _ => None,
            })
        }
    }

    struct H {
        core: Core<Rec>,
        dir: PathBuf,
        t: Instant,
        ms: i64,
    }

    fn secret() -> Arc<Secret> {
        Arc::new(Secret::new(b"test-key".to_vec()))
    }

    fn ok_report(audio: AudioState) -> CaptureReport {
        CaptureReport {
            duration_ms: 2000,
            dropped_chunks: 0,
            problem: None,
            audio,
        }
    }

    fn unconfirmed(text: &str) -> TranscriptOutcome {
        TranscriptOutcome::Incomplete {
            text: text.into(),
            model: LIVE_MODEL,
            reason: Failure::Unconfirmed,
        }
    }

    impl H {
        fn new() -> H {
            H::with(Settings::default(), Some(secret()))
        }

        fn with(settings: Settings, key: Option<Arc<Secret>>) -> H {
            let (store, dir) = store::tests::temp_store();
            let paths = Paths {
                settings: dir.join("settings.json"),
                spool: dir.join("spool"),
                failed: dir.join("failed"),
            };
            std::fs::create_dir_all(&paths.spool).unwrap();
            std::fs::create_dir_all(&paths.failed).unwrap();
            let core = Core::with_runtime(Rec::default(), paths, store, settings, key);
            H {
                core,
                dir,
                t: Instant::now(),
                ms: 1_700_000_000_000,
            }
        }

        fn now(&self) -> Now {
            Now {
                mono: self.t,
                unix_ms: self.ms,
            }
        }

        fn ev(&mut self, e: Event) {
            self.t += Duration::from_millis(1);
            self.ms += 1;
            let now = self.now();
            self.core.handle_at(e, now);
        }

        fn wait(&mut self, d: Duration) {
            self.t += d;
            self.ms += d.as_millis() as i64;
            let now = self.now();
            self.core.on_deadline_at(now);
        }

        fn id(&self) -> SessionId {
            self.core.active_id().expect("an active dictation")
        }

        /// Hotkey → mic opened. Returns the session.
        fn record(&mut self) -> SessionId {
            self.ev(Event::Toggle);
            let id = self.id();
            self.ev(Event::Capture {
                session: id,
                ev: CaptureEvent::Opened,
            });
            assert!(matches!(self.core.state, State::Recording { .. }));
            id
        }

        fn wav(&self, id: SessionId) {
            std::fs::write(self.core.dirs.spool_path(id), vec![1u8; 4000]).unwrap();
        }

        fn captured(&mut self, id: SessionId, audio: AudioState) {
            if audio != AudioState::Absent {
                self.wav(id);
            }
            self.ev(Event::Capture {
                session: id,
                ev: CaptureEvent::Finished(ok_report(audio)),
            });
        }

        fn live(&mut self, id: SessionId, o: TranscriptOutcome) {
            self.ev(Event::Live {
                session: id,
                outcome: o,
            });
        }

        fn rows(&self) -> Vec<store::Row> {
            self.core.store.search("", 50).unwrap()
        }

        fn fx(&self) -> &Rec {
            &self.core.rt
        }

        fn batch_jobs(&self) -> Vec<JobId> {
            self.core
                .rt
                .fx
                .iter()
                .filter_map(|e| match e {
                    Effect::StartBatch { job, .. } => Some(*job),
                    _ => None,
                })
                .collect()
        }
    }

    impl Drop for H {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn text(s: &str) -> BatchText {
        BatchText::Text(NonEmptyText::new(s).unwrap())
    }

    // ------------------------------------------------------------ the normal path

    #[test]
    fn unconfirmed_live_text_is_saved_provisionally_pasted_and_keeps_audio() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(id, unconfirmed("hello world"));
        assert!(h.fx().pastes().is_empty(), "the WAV isn't closed yet");
        h.captured(id, AudioState::Finalized);
        let rows = h.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, store::PROVISIONAL);
        assert_eq!(rows[0].text, "hello world");
        assert!(
            rows[0].audio_path.is_some(),
            "provisional keeps the audio for Retry"
        );
        let pastes = h.fx().pastes();
        assert_eq!(pastes.len(), 1);
        assert_eq!(pastes[0].text, "hello world");
        assert!(
            !pastes[0].incomplete,
            "an unconfirmed stream alone doesn't warn"
        );
        assert!(matches!(h.core.state, State::Idle));
    }

    #[test]
    fn capture_finishing_before_live_waits_for_live() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        assert!(h.rows().is_empty());
        h.live(id, unconfirmed("late live"));
        assert_eq!(h.rows().len(), 1);
        assert_eq!(h.fx().pastes().len(), 1);
    }

    #[test]
    fn retryable_live_failure_falls_back_to_batch_and_completes() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
        );
        h.captured(id, AudioState::Finalized);
        let jobs = h.batch_jobs();
        assert_eq!(jobs.len(), 1, "one fallback batch request");
        h.ev(Event::Batch {
            job: jobs[0],
            result: Ok(text("from batch")),
        });
        let rows = h.rows();
        assert_eq!(rows[0].status, store::OK);
        assert_eq!(rows[0].model.as_deref(), Some(BATCH_MODEL));
        assert!(
            rows[0].audio_path.is_none(),
            "a clean result deletes the recording"
        );
        assert!(!h.core.dirs.spool_path(id).exists());
        assert_eq!(h.fx().pastes()[0].text, "from batch");
        assert!(h.core.jobs.is_empty(), "the slot is released");
    }

    #[test]
    fn batch_failure_keeps_the_live_partial_as_incomplete() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Incomplete {
                text: "half a sent".into(),
                model: LIVE_MODEL,
                reason: Failure::Timeout,
            },
        );
        h.captured(id, AudioState::Finalized);
        let job = h.batch_jobs()[0];
        h.ev(Event::Batch {
            job,
            result: Err(GeminiError::Offline),
        });
        let rows = h.rows();
        assert_eq!(rows[0].status, store::PROVISIONAL);
        assert_eq!(rows[0].text, "half a sent");
        let p = h.fx().pastes();
        assert!(p[0].incomplete, "a real failure warns");
    }

    #[test]
    fn unconfirmed_empty_live_is_resolved_by_batch_silence() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Failed {
                reason: Failure::Unconfirmed,
            },
        );
        h.captured(id, AudioState::Finalized);
        let job = h.batch_jobs()[0];
        h.ev(Event::Batch {
            job,
            result: Ok(BatchText::Empty),
        });
        assert!(h.rows().is_empty(), "recognized silence stores nothing");
        assert!(
            !h.core.dirs.spool_path(id).exists(),
            "and deletes the recording"
        );
        assert_eq!(h.fx().last_text().as_deref(), Some("Nothing heard"));
    }

    #[test]
    fn fallback_is_skipped_when_the_wav_is_not_finalized() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
        );
        h.captured(id, AudioState::Recoverable);
        assert!(h.batch_jobs().is_empty());
        let rows = h.rows();
        assert_eq!(rows[0].status, store::FAILED);
        assert!(rows[0].audio_path.is_some());
    }

    #[test]
    fn busy_batch_slot_makes_fallback_wait_then_settle() {
        let mut h = H::new();
        // The slot is held by a Retry (any job counts).
        h.core.alloc_job(
            Purpose::KeyTest {
                request: None,
                generation: 0,
            },
            Arc::new(AtomicBool::new(false)),
        );
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Incomplete {
                text: "kept words".into(),
                model: LIVE_MODEL,
                reason: Failure::Timeout,
            },
        );
        h.captured(id, AudioState::Finalized);
        assert!(h.batch_jobs().is_empty());
        assert!(matches!(h.core.state, State::Finishing(_)));
        h.wait(BATCH_SLOT_WAIT + Duration::from_millis(1));
        let rows = h.rows();
        assert_eq!(rows.len(), 1, "settled with what it had");
        assert_eq!(rows[0].text, "kept words");
        assert_eq!(rows[0].status, store::PROVISIONAL);
    }

    // ------------------------------------------------------------ adverse orders

    #[test]
    fn lock_before_the_microphone_opened_cancels_the_start() {
        let mut h = H::new();
        h.ev(Event::Toggle);
        let id = h.id();
        h.ev(Event::Power(PowerEvent::Lock));
        assert!(matches!(h.core.state, State::Idle));
        assert!(
            h.fx().captures[0].1.load(Ordering::Acquire),
            "capture told to stop"
        );
        // The microphone opens anyway: nothing starts Live for a dead session.
        h.ev(Event::Capture {
            session: id,
            ev: CaptureEvent::Opened,
        });
        assert_eq!(h.fx().count(|e| matches!(e, Effect::StartLive { .. })), 0);
        // Its (worthless) audio is dropped, not recovered as a dictation.
        h.captured(id, AudioState::Finalized);
        assert!(!h.core.dirs.spool_path(id).exists());
        assert!(h.rows().is_empty());
    }

    #[test]
    fn lock_while_recording_saves_but_never_pastes() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Power(PowerEvent::Lock));
        h.live(id, unconfirmed("secret stuff"));
        h.captured(id, AudioState::Finalized);
        assert!(h.fx().pastes().is_empty());
        let rows = h.rows();
        assert_eq!(rows[0].text, "secret stuff");
        assert!(rows[0].audio_path.is_some());
        // Unlock doesn't revive the withdrawn delivery, and a new session may paste again.
        h.ev(Event::Power(PowerEvent::Unlock));
        let id2 = h.record();
        h.ev(Event::Toggle);
        h.live(id2, unconfirmed("second"));
        h.captured(id2, AudioState::Finalized);
        assert_eq!(h.fx().pastes().len(), 1);
        assert_eq!(h.fx().pastes()[0].text, "second");
    }

    #[test]
    fn lock_while_transcribing_withdraws_the_paste() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.ev(Event::Power(PowerEvent::Lock));
        h.live(id, unconfirmed("words"));
        h.captured(id, AudioState::Finalized);
        assert!(h.fx().pastes().is_empty());
        assert_eq!(h.rows().len(), 1);
    }

    #[test]
    fn quit_while_finishing_waits_then_saves_without_pasting() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.ev(Event::Quit);
        assert!(h.core.quit_by.is_some());
        h.live(id, unconfirmed("goodbye"));
        h.captured(id, AudioState::Finalized);
        assert!(h.fx().pastes().is_empty());
        assert_eq!(h.rows()[0].text, "goodbye");
        assert!(matches!(h.core.state, State::Idle));
    }

    #[test]
    fn quit_deadline_drops_a_dictation_that_never_finished() {
        let mut h = H::new();
        h.record();
        h.ev(Event::Toggle);
        h.ev(Event::Quit);
        h.wait(QUIT_WAIT + Duration::from_millis(1));
        assert!(matches!(h.core.state, State::Idle));
    }

    #[test]
    fn a_second_press_cancels_and_a_stale_first_press_does_not() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle); // stop
        h.ev(Event::Toggle); // arms
        assert!(matches!(h.core.state, State::Finishing(_)));
        h.t += CANCEL_WINDOW + Duration::from_secs(1);
        h.ev(Event::Toggle); // too late: arms again, doesn't cancel
        assert!(matches!(h.core.state, State::Finishing(_)));
        h.ev(Event::Toggle); // within the window: cancels
        // The capture hasn't reported: the dictation is parked, never renamed.
        assert!(matches!(h.core.state, State::Idle));
        assert!(h.core.retiring.is_some());
        h.captured(id, AudioState::Finalized);
        assert!(h.core.retiring.is_none());
        let rows = h.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, store::FAILED);
        assert!(rows[0].error.as_deref().unwrap().contains("Cancelled"));
        assert!(
            rows[0].audio_path.is_some(),
            "Retry can still use the audio"
        );
        assert!(h.fx().pastes().is_empty());
    }

    #[test]
    fn cancel_keeps_text_already_heard() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::LiveText {
            session: id,
            finals: "so far".into(),
            interim: "we have".into(),
        });
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        let rows = h.rows();
        assert_eq!(rows[0].text, "so far we have");
        assert_eq!(rows[0].status, store::PROVISIONAL);
        assert!(h.fx().pastes().is_empty());
    }

    #[test]
    fn a_new_recording_is_refused_while_an_abandoned_one_is_still_closing() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        assert!(h.core.retiring.is_some());
        h.ev(Event::Toggle);
        assert!(
            matches!(h.core.state, State::Idle),
            "no second capture while one is retiring"
        );
        assert_eq!(h.fx().captures.len(), 1);
        assert_eq!(
            h.fx().last_text().as_deref(),
            Some("Still saving the last recording")
        );
        h.captured(id, AudioState::Finalized);
        h.ev(Event::Toggle);
        assert!(matches!(h.core.state, State::Starting { .. }));
    }

    #[test]
    fn a_retiring_capture_that_never_reports_does_not_block_forever() {
        let mut h = H::new();
        h.record();
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        assert!(h.core.retiring.is_some());
        h.wait(FINISH_WAIT + Duration::from_secs(1));
        assert!(h.core.retiring.is_none());
    }

    #[test]
    fn the_timeout_abandons_without_pasting() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        h.wait(FINISH_WAIT + Duration::from_secs(1));
        let rows = h.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, store::FAILED);
        assert!(rows[0].audio_path.is_some());
        assert!(h.fx().pastes().is_empty());
    }

    #[test]
    fn a_late_live_result_fills_in_the_row_saved_empty() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        h.wait(FINISH_WAIT + Duration::from_secs(1)); // gave up while Live was still running
        assert!(h.core.late_rows.contains_key(&id));
        h.live(id, unconfirmed("it did arrive"));
        let rows = h.rows();
        assert_eq!(rows[0].text, "it did arrive");
        assert_eq!(rows[0].status, store::PROVISIONAL);
        assert!(h.core.late_rows.is_empty());
        assert!(h.fx().pastes().is_empty(), "never pasted late");
    }

    #[test]
    fn late_events_for_a_finished_session_cannot_draw_over_a_new_recording() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(id, unconfirmed("one"));
        h.captured(id, AudioState::Finalized);
        let first_row = h.rows()[0].id;
        let id2 = h.record();
        let before = h.fx().presents().len();
        h.ev(Event::LiveText {
            session: id,
            finals: "stale".into(),
            interim: String::new(),
        });
        h.ev(Event::Pasted(paste::Pasted {
            session: id,
            row_id: first_row,
            incomplete: false,
            outcome: Outcome::Attempted,
        }));
        h.ev(Event::Pasted(paste::Pasted {
            session: id,
            row_id: first_row,
            incomplete: true,
            outcome: Outcome::Failed("clipboard"),
        }));
        assert_eq!(
            h.fx().presents().len(),
            before,
            "nothing from the old session reached the HUD"
        );
        assert_eq!(h.core.hud_owner, Some(HudOwner::Dictation(id2)));
        // The result is still recorded against the old row.
        assert_eq!(
            h.core
                .store
                .get(first_row)
                .unwrap()
                .unwrap()
                .paste
                .as_deref(),
            Some("failed:clipboard")
        );
    }

    #[test]
    fn microphone_loss_while_recording_finishes_with_what_was_captured() {
        let mut h = H::new();
        let id = h.record();
        h.wav(id);
        h.ev(Event::Capture {
            session: id,
            ev: CaptureEvent::Finished(CaptureReport {
                duration_ms: 1500,
                dropped_chunks: 0,
                problem: Some("device unplugged".into()),
                audio: AudioState::Finalized,
            }),
        });
        assert!(matches!(h.core.state, State::Finishing(_)));
        h.live(id, unconfirmed("before it died"));
        let rows = h.rows();
        assert_eq!(
            rows[0].status,
            store::PROVISIONAL,
            "capture trouble is never clean"
        );
        assert!(
            rows[0]
                .error
                .as_deref()
                .unwrap()
                .contains("device unplugged")
        );
        assert!(rows[0].audio_path.is_some());
    }

    #[test]
    fn live_ending_early_is_decided_when_the_user_stops() {
        let mut h = H::new();
        let id = h.record();
        h.live(
            id,
            TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
        );
        assert!(matches!(h.core.state, State::Recording { .. }));
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        assert_eq!(
            h.batch_jobs().len(),
            1,
            "falls back to batch once audio is final"
        );
    }

    #[test]
    fn capture_that_cannot_start_leaves_idle_and_releases_everything() {
        let mut h = H::new();
        h.core.rt.fail_capture = true;
        h.ev(Event::Toggle);
        assert!(matches!(h.core.state, State::Idle));
        assert_eq!(
            h.fx().last_text().as_deref(),
            Some("Couldn't start the microphone")
        );
        h.core.rt.fail_capture = false;
        h.ev(Event::Toggle);
        assert!(matches!(h.core.state, State::Starting { .. }));
    }

    #[test]
    fn busy_microphone_is_reported_and_nothing_starts() {
        let mut h = H::new();
        h.core.rt.busy = true;
        h.ev(Event::Toggle);
        assert!(matches!(h.core.state, State::Idle));
        assert_eq!(
            h.fx().last_text().as_deref(),
            Some("Microphone still stuck — replug it")
        );
    }

    #[test]
    fn missing_key_blocks_a_start() {
        let mut h = H::with(Settings::default(), None);
        h.ev(Event::Toggle);
        assert!(matches!(h.core.state, State::Idle));
        assert_eq!(h.fx().captures.len(), 0);
    }

    #[test]
    fn a_microphone_that_never_opens_times_out_quietly() {
        let mut h = H::new();
        h.ev(Event::Toggle);
        h.wait(OPEN_TIMEOUT + Duration::from_millis(1));
        assert!(matches!(h.core.state, State::Idle));
        assert_eq!(
            h.fx().last_text().as_deref(),
            Some("Microphone didn't respond")
        );
    }

    #[test]
    fn live_spawn_failure_still_ends_in_a_stored_result() {
        let mut h = H::new();
        h.core.rt.fail_live = true;
        let id = h.record();
        assert!(
            h.core.live_slot.is_none(),
            "no slot held for a worker that never existed"
        );
        h.ev(Event::Toggle);
        h.captured(id, AudioState::Finalized);
        assert_eq!(h.batch_jobs().len(), 1, "batch takes over");
    }

    #[test]
    fn batch_spawn_failure_frees_the_slot_and_stores_a_failed_row() {
        let mut h = H::new();
        h.core.rt.fail_batch = true;
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(
            id,
            TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
        );
        h.captured(id, AudioState::Finalized);
        assert!(h.core.jobs.is_empty());
        assert_eq!(h.rows()[0].status, store::FAILED);
    }

    #[test]
    fn paste_worker_missing_marks_the_row_and_does_not_wedge_notices() {
        let mut h = H::new();
        h.core.rt.fail_paste = true;
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(id, unconfirmed("text"));
        h.captured(id, AudioState::Finalized);
        assert!(h.core.paste_pending.is_none());
        assert_eq!(h.rows()[0].paste.as_deref(), Some("failed:worker"));
    }

    #[test]
    fn an_orphan_capture_report_with_audio_is_left_for_recovery() {
        let mut h = H::new();
        let ghost = SessionId(42);
        h.wav(ghost);
        h.captured(ghost, AudioState::Finalized);
        assert!(h.core.dirs.spool_path(ghost).exists());
        assert!(h.rows().is_empty());
    }

    #[test]
    fn session_ids_never_repeat_even_if_the_clock_stalls_or_runs_backwards() {
        let mut h = H::new();
        let a = h.core.next_session().unwrap();
        h.ms -= 10_000;
        let b = h.core.next_session().unwrap();
        let c = h.core.next_session().unwrap();
        assert!(a < b && b < c);
        h.core.last_session = i64::MAX as u64;
        assert!(h.core.next_session().is_none(), "never beyond i64::MAX");
    }

    // ------------------------------------------------------------ the HUD

    #[test]
    fn notices_wait_while_a_dictation_owns_the_hud() {
        let mut h = H::new();
        h.record();
        let before = h.fx().presents().len();
        h.core.notice("some background news", Tone::Info);
        assert_eq!(h.fx().presents().len(), before);
    }

    #[test]
    fn notices_wait_while_a_paste_is_pending_then_pasted_result_shows() {
        let mut h = H::new();
        let id = h.record();
        h.ev(Event::Toggle);
        h.live(id, unconfirmed("text"));
        h.captured(id, AudioState::Finalized);
        assert_eq!(h.core.paste_pending, Some(id));
        let before = h.fx().presents().len();
        h.core.notice("background", Tone::Info);
        assert_eq!(h.fx().presents().len(), before);
        let row = h.rows()[0].id;
        h.ev(Event::Pasted(paste::Pasted {
            session: id,
            row_id: row,
            incomplete: false,
            outcome: Outcome::Attempted,
        }));
        assert!(h.core.paste_pending.is_none());
        assert_eq!(
            h.core.store.get(row).unwrap().unwrap().paste.as_deref(),
            Some("attempted")
        );
        assert!(matches!(
            h.fx().presents().last().unwrap().command,
            HudCommand::Done
        ));
    }

    #[test]
    fn every_present_carries_the_epoch_of_its_owner() {
        let mut h = H::new();
        h.record();
        let epochs: Vec<u64> = h.fx().presents().iter().map(|p| p.epoch).collect();
        assert!(epochs.windows(2).all(|w| w[0] == w[1]), "{epochs:?}");
        let first = epochs[0];
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        h.ev(Event::Toggle);
        let id = h.id_or_retiring();
        h.captured(id, AudioState::Finalized);
        h.record();
        assert!(h.fx().presents().last().unwrap().epoch > first);
    }

    impl H {
        fn id_or_retiring(&self) -> SessionId {
            self.core
                .retiring
                .as_ref()
                .map(|f| f.ctx.id)
                .or_else(|| self.core.active_id())
                .unwrap()
        }
    }

    #[test]
    fn the_tray_follows_the_recording_state() {
        let mut h = H::new();
        h.record();
        assert!(h.core.tray_on);
        h.ev(Event::Toggle);
        assert!(!h.core.tray_on);
        let on = h.fx().count(|e| matches!(e, Effect::RecordingTray(true)));
        let off = h.fx().count(|e| matches!(e, Effect::RecordingTray(false)));
        assert_eq!((on, off), (1, 1));
    }

    // ------------------------------------------------------------ Retry, history, UI

    fn seed_failed(h: &mut H, with_audio: bool) -> (i64, PathBuf) {
        let id = SessionId(900);
        let audio = h.core.dirs.failed_path(id);
        std::fs::write(&audio, vec![2u8; 3000]).unwrap();
        let path = audio.to_string_lossy().into_owned();
        let row = h
            .core
            .store
            .insert(&NewRow {
                created_ms: 5,
                text: "",
                status: store::FAILED,
                error: Some("boom"),
                audio_path: with_audio.then_some(path.as_str()),
                ..Default::default()
            })
            .unwrap();
        (row, audio)
    }

    fn request(h: &mut H, action: Action) -> RequestId {
        let id = RequestId::next();
        h.ev(Event::Ui(ActionRequest {
            id,
            window_generation: 3,
            action,
        }));
        id
    }

    fn reply_for(h: &H, id: RequestId) -> Vec<Result<ActionSuccess, ActionFailure>> {
        h.fx()
            .results()
            .into_iter()
            .filter(|r| r.id == id)
            .map(|r| {
                assert_eq!(r.window_generation, 3);
                r.result.clone()
            })
            .collect()
    }

    #[test]
    fn retry_success_updates_the_row_and_deletes_the_audio() {
        let mut h = H::new();
        let (row, audio) = seed_failed(&mut h, true);
        let req = request(&mut h, Action::Retry(row));
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::Done)]);
        let job = h.batch_jobs()[0];
        assert!(h.core.by_row.contains_key(&row));
        h.ev(Event::Batch {
            job,
            result: Ok(text("fixed")),
        });
        let r = h.core.store.get(row).unwrap().unwrap();
        assert_eq!((r.text.as_str(), r.status.as_str()), ("fixed", store::OK));
        assert!(r.audio_path.is_none());
        assert!(!audio.exists());
        assert!(h.core.jobs.is_empty() && h.core.by_row.is_empty());
    }

    #[test]
    fn retry_is_refused_while_recording_or_when_the_slot_is_busy() {
        let mut h = H::new();
        let (row, _) = seed_failed(&mut h, true);
        h.record();
        let req = request(&mut h, Action::Retry(row));
        assert!(reply_for(&h, req)[0].is_err());
        assert!(h.batch_jobs().is_empty());
        h.ev(Event::Toggle);
        let id = h.id();
        h.live(id, unconfirmed("x"));
        h.captured(id, AudioState::Finalized);
        // Occupy the slot with a key check, then try again.
        let kt = request(&mut h, Action::TestKey);
        assert!(
            reply_for(&h, kt).is_empty(),
            "the key check replies when it finishes"
        );
        let req = request(&mut h, Action::Retry(row));
        assert!(reply_for(&h, req)[0].is_err());
        assert_eq!(h.core.jobs.len(), 1);
    }

    #[test]
    fn deleting_a_row_cancels_its_retry_and_defers_the_audio() {
        let mut h = H::new();
        let (row, audio) = seed_failed(&mut h, true);
        request(&mut h, Action::Retry(row));
        let job = h.batch_jobs()[0];
        let cancel = h.core.jobs[&job].cancel.clone();
        let req = request(&mut h, Action::Delete(row));
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::Done)]);
        assert!(cancel.load(Ordering::Acquire));
        assert!(audio.exists(), "the worker is still reading it");
        h.ev(Event::Batch {
            job,
            result: Ok(text("too late")),
        });
        assert!(!audio.exists(), "removed once the worker exited");
        assert!(
            h.core.store.get(row).unwrap().is_none(),
            "the row stays deleted"
        );
        assert!(h.core.store.tombstones().unwrap().is_empty());
        assert!(h.core.jobs.is_empty());
    }

    #[test]
    fn retry_failure_records_the_reason_and_keeps_the_audio() {
        let mut h = H::new();
        let (row, audio) = seed_failed(&mut h, true);
        request(&mut h, Action::Retry(row));
        let job = h.batch_jobs()[0];
        h.ev(Event::Batch {
            job,
            result: Err(GeminiError::RateLimited),
        });
        let r = h.core.store.get(row).unwrap().unwrap();
        assert!(r.error.unwrap().contains("rate limit"));
        assert!(audio.exists() && r.audio_path.is_some());
    }

    #[test]
    fn retry_without_audio_is_a_failure_reply() {
        let mut h = H::new();
        let (row, _) = seed_failed(&mut h, false);
        let req = request(&mut h, Action::Retry(row));
        assert!(reply_for(&h, req)[0].is_err());
    }

    #[test]
    fn key_check_replies_once_and_a_changed_key_is_flagged() {
        let mut h = H::new();
        let req = request(&mut h, Action::TestKey);
        let job = h.core.jobs.keys().next().copied().unwrap();
        h.core.key_generation += 1; // the key was replaced mid-check
        h.ev(Event::KeyTest {
            job,
            result: Ok(()),
        });
        let r = reply_for(&h, req);
        assert_eq!(r.len(), 1);
        assert!(
            r[0].is_err(),
            "a result for an older key is not a result for this one"
        );
        assert!(h.core.jobs.is_empty());
        // And a good check on the current key succeeds.
        let req = request(&mut h, Action::TestKey);
        let job = h.core.jobs.keys().next().copied().unwrap();
        h.ev(Event::KeyTest {
            job,
            result: Ok(()),
        });
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::KeyChecked)]);
    }

    #[test]
    fn copy_reports_success_only_after_the_clipboard_took_it() {
        let mut h = H::new();
        let (row, _) = seed_failed(&mut h, true);
        h.core
            .store
            .apply_late_text(row, "copy me", "m", None)
            .unwrap();
        let req = request(&mut h, Action::Copy(row));
        assert!(reply_for(&h, req).is_empty());
        h.ev(Event::Copied {
            request: req,
            result: Err(fail("The clipboard is busy")),
        });
        assert_eq!(reply_for(&h, req).len(), 1);
        assert!(reply_for(&h, req)[0].is_err());
        let req2 = request(&mut h, Action::Copy(row));
        h.ev(Event::Copied {
            request: req2,
            result: Ok(()),
        });
        assert_eq!(reply_for(&h, req2), vec![Ok(ActionSuccess::Copied)]);
        // A reply for a request the core never made is ignored.
        h.ev(Event::Copied {
            request: RequestId::next(),
            result: Ok(()),
        });
    }

    #[test]
    fn dictionary_reply_follows_the_commit() {
        let mut h = H::new();
        let req = request(
            &mut h,
            Action::SetDictionary(vec!["Zed".into(), " ".into()]),
        );
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::DictionarySaved)]);
        assert_eq!(h.core.store.dictionary().unwrap(), vec!["Zed".to_string()]);
    }

    #[test]
    fn settings_reach_the_running_core_only_after_they_are_durable() {
        let mut h = H::new();
        let s = Settings {
            sounds: false,
            keep_days: 0,
            ..Settings::default()
        };
        let req = request(&mut h, Action::SaveSettings(s));
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::SettingsSaved)]);
        assert!(!h.core.settings.sounds);
        assert!(!Settings::load(&h.core.paths.settings).sounds);

        // A path that can't be written: reported, and the running settings stay as they were.
        h.core.paths.settings = h.dir.join("no-such-dir").join("settings.json");
        let before = h.fx().count(|e| matches!(e, Effect::SetChord(_)));
        let s2 = Settings {
            sounds: true,
            ..Settings::default()
        };
        let req = request(&mut h, Action::SaveSettings(s2));
        assert!(reply_for(&h, req)[0].is_err());
        assert!(!h.core.settings.sounds, "unchanged");
        assert_eq!(h.fx().count(|e| matches!(e, Effect::SetChord(_))), before);
    }

    #[test]
    fn clear_before_removes_rows_and_audio() {
        let mut h = H::new();
        let (row, audio) = seed_failed(&mut h, true);
        let req = request(&mut h, Action::ClearBefore(i64::MAX));
        assert_eq!(reply_for(&h, req), vec![Ok(ActionSuccess::Done)]);
        assert!(h.core.store.get(row).unwrap().is_none());
        assert!(!audio.exists());
    }
    #[test]
    fn key_adoption_is_durable_first_and_does_not_reread_credentials() {
        let mut h = H::new();
        let old = h.core.key.as_ref().unwrap().as_str().unwrap().to_string();
        h.core.rt.fail_key = true;
        assert!(h.core.set_key("new-synthetic-key").is_err());
        assert_eq!(h.core.key.as_ref().unwrap().as_str(), Some(old.as_str()));
        h.core.rt.fail_key = false;
        assert!(h.core.set_key("new-synthetic-key").is_ok());
        assert_eq!(
            h.core.key.as_ref().unwrap().as_str(),
            Some("new-synthetic-key")
        );
    }
    #[test]
    fn copy_latest_skips_empty_newer_rows_and_waits_for_clipboard_success() {
        let mut h = H::new();
        let wanted = h
            .core
            .store
            .insert(&NewRow {
                created_ms: 1,
                text: "last synthetic text",
                status: store::OK,
                ..Default::default()
            })
            .unwrap();
        h.core
            .store
            .insert(&NewRow {
                created_ms: 2,
                status: store::FAILED,
                ..Default::default()
            })
            .unwrap();
        let id = request(&mut h, Action::CopyLatest);
        assert!(reply_for(&h, id).is_empty());
        assert!(h.fx().fx.iter().any(|e| matches!(e, Effect::Copy { request, text } if *request == id && text == "last synthetic text")));
        assert_eq!(h.core.store.latest_text().unwrap().unwrap().id, wanted);
        h.ev(Event::Copied {
            request: id,
            result: Ok(()),
        });
        assert_eq!(reply_for(&h, id), vec![Ok(ActionSuccess::Copied)]);
    }
}
