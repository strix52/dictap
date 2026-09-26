//! The core loop: owns the dictation state, the write DB connection, settings and key.
//! Everything arrives as an `Event`; per-dictation events carry a `sid` and stale ones
//! are dropped.

use crate::capture::{self, Capture};
use crate::event::{CaptureEvent, Event, LiveEvent, PowerEvent, UiCmd};
use crate::gemini::{GeminiError, batch, live};
use crate::key::Secret;
use crate::paste::{self, Job, PasteJob};
use crate::settings::Settings;
use crate::store::{self, NewRow, Store};
use crate::win::overlay::{self, Tone};
use crate::win::window::{self, Window};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
const NOTICE: Duration = Duration::from_secs(4);
const KEEP_FILES: usize = 20;
const KEEP_BYTES: u64 = 200 << 20;

pub struct Paths {
    pub settings: PathBuf,
    pub spool: PathBuf,
    pub failed: PathBuf,
}

enum State {
    Idle,
    Starting { cap: Capture, deadline: Instant },
    Recording { cap: Capture, opened: Instant },
    Finishing(Finish),
}

/// A stopped dictation waiting for capture to finalize and for Live (then maybe batch).
struct Finish {
    target: Option<Window>,
    duration_ms: Option<u64>,
    capture_reason: Option<String>,
    live: Option<LiveEvent>,
    batch_started: bool,
}

pub struct Core {
    paths: Paths,
    store: Store,
    settings: Settings,
    key: Option<Arc<Secret>>,
    events: Sender<Event>,
    paste: Sender<Job>,
    state: State,
    sid: u64,
    started_ms: i64,
    /// Retry sid → row id.
    retries: HashMap<u64, i64>,
    /// Live's verdict if it arrived while still recording.
    pending_live: Option<LiveEvent>,
    quitting: bool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn notice(text: &str, tone: Tone) {
    overlay::show(text, tone, Some(NOTICE));
}

impl Core {
    pub fn new(paths: Paths, store: Store, events: Sender<Event>) -> Core {
        let settings = Settings::load(&paths.settings);
        crate::win::hook::set_chord(settings.chord());
        let _ = std::fs::create_dir_all(&paths.spool);
        let _ = std::fs::create_dir_all(&paths.failed);
        let paste = paste::spawn(events.clone());
        let mut core = Core {
            paths,
            store,
            settings,
            key: crate::key::load().map(Arc::new),
            events,
            paste,
            state: State::Idle,
            sid: 0,
            started_ms: 0,
            retries: HashMap::new(),
            pending_live: None,
            quitting: false,
        };
        core.recover_spool();
        core.first_run_import();
        log::info!(
            "core: key {}",
            if core.key.is_some() {
                "present"
            } else {
                "missing"
            }
        );
        core
    }

    pub fn run(mut self, rx: Receiver<Event>) {
        loop {
            let ev = match self.deadline() {
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
                Some(ev) => self.handle(ev),
                None => self.on_deadline(),
            }
            if self.quitting && matches!(self.state, State::Idle) {
                return;
            }
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Starting { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }

    fn on_deadline(&mut self) {
        if let State::Starting { cap, deadline } = &self.state
            && Instant::now() >= *deadline
        {
            cap.stop();
            self.state = State::Idle;
            log::warn!("core {}: microphone didn't open in time", self.sid);
            notice("Microphone didn't respond", Tone::Error);
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Toggle => self.toggle(),
            Event::ShowHistory => {
                // History window arrives with the UI; until then say so.
                notice("History window isn't built yet", Tone::Info);
            }
            Event::Power(p) => {
                log::info!("power: {p:?}");
                if matches!(p, PowerEvent::Suspend | PowerEvent::Lock) {
                    self.stop_recording();
                }
            }
            Event::Ui(cmd) => self.ui(cmd),
            Event::Capture { sid, ev } => self.capture_event(sid, ev),
            Event::Live { sid, ev } => {
                if let Some(row) = self.retries.remove(&sid) {
                    self.retry_done(row, ev);
                } else if sid == self.sid {
                    self.live_event(ev);
                }
            }
            Event::Pasted(p) => {
                if let Err(e) = self.store.set_paste(p.row_id, &p.outcome.db_value()) {
                    log::error!("store paste: {e}");
                }
                match p.outcome.notice() {
                    Some(n) => notice(n, Tone::Error),
                    None => overlay::hide(),
                }
            }
            Event::Quit => {
                self.quitting = true;
                match &self.state {
                    State::Starting { cap, .. } => {
                        cap.stop();
                        self.state = State::Idle;
                    }
                    State::Recording { .. } => self.stop_recording(),
                    _ => {}
                }
            }
        }
    }

    fn next_sid(&mut self) -> u64 {
        self.sid = (now_ms() as u64).max(self.sid + 1);
        self.sid
    }

    fn spool_path(&self, sid: u64) -> PathBuf {
        self.paths.spool.join(format!("{sid}.wav"))
    }

    fn toggle(&mut self) {
        match &self.state {
            State::Idle => self.start(),
            State::Starting { cap, .. } => {
                cap.stop(); // nothing said yet; the thread exits quietly
                self.state = State::Idle;
                overlay::hide();
            }
            State::Recording { .. } => self.stop_recording(),
            State::Finishing(_) => notice("Still transcribing the last one…", Tone::Busy),
        }
    }

    fn start(&mut self) {
        if self.key.is_none() {
            notice(&GeminiError::KeyMissing.to_string(), Tone::Error);
            return;
        }
        if capture::busy() {
            notice("Microphone still stuck — replug it", Tone::Error);
            return;
        }
        let sid = self.next_sid();
        self.started_ms = now_ms();
        self.pending_live = None;
        let cap = capture::start(sid, self.spool_path(sid), self.events.clone());
        self.state = State::Starting {
            cap,
            deadline: Instant::now() + OPEN_TIMEOUT,
        };
        overlay::show("Starting…", Tone::Busy, None);
    }

    fn stop_recording(&mut self) {
        let State::Recording { cap, opened } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return;
        };
        cap.stop();
        let target = window::foreground().filter(|&w| !window::is_own(w));
        log::info!(
            "core {}: stop after {} ms",
            self.sid,
            opened.elapsed().as_millis()
        );
        self.state = State::Finishing(Finish {
            target,
            duration_ms: None,
            capture_reason: None,
            live: None,
            batch_started: false,
        });
        overlay::show("Transcribing…", Tone::Busy, None);
    }

    fn capture_event(&mut self, sid: u64, ev: CaptureEvent) {
        if sid != self.sid {
            // A cancelled capture that still opened: its audio isn't wanted.
            if let CaptureEvent::Ended { .. } = ev {
                let _ = std::fs::remove_file(self.spool_path(sid));
            }
            return;
        }
        match ev {
            CaptureEvent::Opened => {
                let State::Starting { cap, .. } = std::mem::replace(&mut self.state, State::Idle)
                else {
                    return;
                };
                let params = live::Params {
                    key: self.key.clone().expect("checked at start"),
                    language: self.settings.language().map(str::to_string),
                    words: self.store.dictionary().unwrap_or_default(),
                };
                live::spawn(sid, params, cap.queue.clone(), self.events.clone());
                self.state = State::Recording {
                    cap,
                    opened: Instant::now(),
                };
                overlay::show("Listening…", Tone::Recording, None);
            }
            CaptureEvent::Failed(e) => {
                if matches!(self.state, State::Starting { .. } | State::Recording { .. }) {
                    self.state = State::Idle;
                    notice(&e, Tone::Error);
                }
            }
            CaptureEvent::Ended {
                duration_ms,
                dropped,
                reason,
            } => {
                log::info!("core {sid}: captured {duration_ms} ms, dropped {dropped}, {reason:?}");
                if matches!(self.state, State::Recording { .. }) {
                    self.stop_recording(); // mic unplugged or errored
                }
                if let State::Finishing(f) = &mut self.state {
                    f.duration_ms = Some(duration_ms);
                    f.capture_reason = reason;
                }
                self.advance();
            }
        }
    }

    fn live_event(&mut self, ev: LiveEvent) {
        match &mut self.state {
            State::Finishing(f) => f.live = Some(ev),
            // Live finished or failed while still recording (e.g. socket dropped): keep
            // recording; the result is decided once the user stops.
            State::Recording { .. } => {
                if let LiveEvent::Failed { error, .. } = &ev {
                    log::warn!("core {}: live failed mid-recording: {error:?}", self.sid);
                }
                self.pending_live = Some(ev);
                return;
            }
            _ => return,
        }
        self.advance();
    }

    /// Moves a finishing dictation forward once both capture and Live have reported.
    fn advance(&mut self) {
        let State::Finishing(f) = &mut self.state else {
            return;
        };
        if f.live.is_none() {
            f.live = self.pending_live.take();
        }
        let (Some(duration_ms), Some(live)) = (f.duration_ms, f.live.take()) else {
            return;
        };
        let sid = self.sid;
        match live {
            LiveEvent::Failed { error, partial } if error.is_retryable() && !f.batch_started => {
                f.batch_started = true;
                log::info!("core {sid}: live failed ({error:?}); batch fallback");
                let job = batch::Job {
                    key: self.key.clone().expect("checked at start"),
                    language: self.settings.language().map(str::to_string),
                    words: self.store.dictionary().unwrap_or_default(),
                    wav: self.spool_path(sid),
                    partial,
                };
                batch::spawn(sid, job, self.events.clone());
            }
            LiveEvent::Failed { error, partial } => {
                let provisional = !partial.is_empty();
                self.commit(duration_ms, partial, provisional, None, Some(error));
            }
            LiveEvent::Done {
                text,
                provisional,
                model,
                error,
            } => {
                self.commit(duration_ms, text, provisional, Some(model), error);
            }
        }
    }

    fn commit(
        &mut self,
        duration_ms: u64,
        text: String,
        provisional: bool,
        model: Option<&'static str>,
        error: Option<GeminiError>,
    ) {
        let State::Finishing(f) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let sid = self.sid;
        let spool = self.spool_path(sid);
        if text.is_empty() && error.is_none() {
            let _ = std::fs::remove_file(&spool);
            notice("Nothing heard", Tone::Info);
            return;
        }
        let status = if error.is_some() && text.is_empty() {
            store::FAILED
        } else if provisional || error.is_some() {
            store::PROVISIONAL
        } else {
            store::OK
        };
        // Keep the audio for anything short of a clean result, so Retry can fix it.
        let kept = (status != store::OK).then(|| self.paths.failed.join(format!("{sid}.wav")));
        if let Some(k) = &kept
            && let Err(e) = std::fs::rename(&spool, k)
        {
            log::error!("core {sid}: keeping audio failed: {e}");
        }
        let error_text = error
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| f.capture_reason.clone());
        let kept_str = kept.as_ref().map(|k| k.to_string_lossy().into_owned());
        let row = NewRow {
            created_ms: self.started_ms,
            text: &text,
            duration_ms: Some(duration_ms as i64),
            model,
            status,
            error: error_text.as_deref(),
            audio_path: kept_str.as_deref(),
        };
        let id = match self.store.insert(&row) {
            Ok(id) => id,
            Err(e) => {
                // The WAV stays on disk; startup recovery turns it into a row.
                log::error!("core {sid}: history insert failed: {e}");
                if let Some(k) = &kept {
                    let _ = std::fs::rename(k, &spool);
                }
                notice("Couldn't save to history", Tone::Error);
                return;
            }
        };
        if kept.is_none() {
            let _ = std::fs::remove_file(&spool);
        } else {
            self.enforce_retention();
        }
        log::info!("core {sid}: row {id} {status}, {} chars", text.len());

        if text.is_empty() {
            let msg = error.map_or("Transcription failed".into(), |e| e.to_string());
            notice(&format!("{msg} — audio saved in history"), Tone::Error);
            return;
        }
        if let Some(e) = &error {
            log::warn!("core {sid}: provisional result: {e}");
        }
        let _ = self.paste.send(Job::Paste(PasteJob {
            row_id: id,
            text,
            target: f.target,
        }));
    }

    fn retry(&mut self, id: i64) {
        let Some(key) = self.key.clone() else {
            notice(&GeminiError::KeyMissing.to_string(), Tone::Error);
            return;
        };
        let Ok(Some(row)) = self.store.get(id) else {
            return;
        };
        let Some(path) = row.audio_path else {
            notice("No audio kept for that one", Tone::Info);
            return;
        };
        let sid = self.next_sid();
        self.retries.insert(sid, id);
        let job = batch::Job {
            key,
            language: self.settings.language().map(str::to_string),
            words: self.store.dictionary().unwrap_or_default(),
            wav: PathBuf::from(path),
            partial: String::new(),
        };
        batch::spawn(sid, job, self.events.clone());
        overlay::show("Retrying…", Tone::Busy, None);
    }

    fn retry_done(&mut self, id: i64, ev: LiveEvent) {
        let (text, error) = match ev {
            LiveEvent::Done { text, error, .. } => (text, error),
            LiveEvent::Failed { error, .. } => (String::new(), Some(error)),
        };
        let row = self.store.get(id).ok().flatten();
        match error {
            None => {
                let path = row.and_then(|r| r.audio_path);
                match self
                    .store
                    .set_result(id, Some(&text), store::OK, None, None)
                {
                    Ok(()) => {
                        if let Some(p) = path {
                            let _ = std::fs::remove_file(p);
                        }
                        notice("Retried — saved to history", Tone::Info);
                    }
                    Err(e) => log::error!("retry {id}: {e}"),
                }
            }
            Some(e) => {
                let status = row.as_ref().map_or(store::FAILED, |r| r.status.as_str());
                let path = row.as_ref().and_then(|r| r.audio_path.as_deref());
                let _ = self
                    .store
                    .set_result(id, None, status, Some(&e.to_string()), path);
                notice(&e.to_string(), Tone::Error);
            }
        }
    }

    fn ui(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::Copy(id) => {
                if let Ok(Some(row)) = self.store.get(id) {
                    let _ = self.paste.send(Job::Copy(row.text));
                }
            }
            UiCmd::Retry(id) => self.retry(id),
            UiCmd::Delete(id) => match self.store.delete(id) {
                Ok(Some(path)) => {
                    let _ = std::fs::remove_file(path);
                }
                Ok(None) => {}
                Err(e) => log::error!("delete {id}: {e}"),
            },
            UiCmd::SaveSettings(s) => {
                crate::win::hook::set_chord(s.chord());
                if let Err(e) = s.save(&self.paths.settings) {
                    log::error!("settings save: {e}");
                }
                self.settings = s;
            }
            UiCmd::SetDictionary(words) => {
                if let Err(e) = self.store.set_dictionary(&words) {
                    log::error!("dictionary: {e}");
                }
            }
            UiCmd::SetApiKey(k) => {
                let k = Secret::new(k.into_bytes());
                match crate::key::store(k.as_str().unwrap_or("")) {
                    Ok(()) => {
                        self.key = crate::key::load().map(Arc::new);
                        self.test_key();
                    }
                    Err(e) => log::error!("key store: {e}"),
                }
            }
            UiCmd::TestKey => self.test_key(),
            UiCmd::ImportOpenWhispr => self.import_openwhispr(),
            UiCmd::SetAutostart(on) => log::info!("autostart {on}: not built yet"),
        }
    }

    fn test_key(&self) {
        let Some(key) = self.key.clone() else {
            notice(&GeminiError::KeyMissing.to_string(), Tone::Error);
            return;
        };
        std::thread::spawn(move || match batch::test_key(key.as_str().unwrap_or("")) {
            Ok(()) => notice("Gemini key works", Tone::Info),
            Err(e) => notice(&e.to_string(), Tone::Error),
        });
    }

    fn first_run_import(&mut self) {
        if self
            .store
            .meta("openwhispr_import_ms")
            .ok()
            .flatten()
            .is_none()
            && crate::import::openwhispr_dir().is_some()
        {
            self.import_openwhispr();
        }
    }

    fn import_openwhispr(&mut self) {
        let Some(dir) = crate::import::openwhispr_dir() else {
            notice("OpenWhispr data not found", Tone::Info);
            return;
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
        match history {
            Ok(r) => notice(
                &format!("Imported {} from OpenWhispr{key_note}", r.imported),
                Tone::Info,
            ),
            Err(e) => notice(&e, Tone::Error),
        }
    }

    /// Leftover spool files are dictations interrupted by a crash: keep them as failed rows.
    fn recover_spool(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.paths.spool) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "wav") {
                continue;
            }
            let Some(sid) = path
                .file_stem()
                .and_then(|s| s.to_str()?.parse::<i64>().ok())
            else {
                continue;
            };
            let duration = crate::audio::repair(&path).ok();
            if duration.is_none_or(|d| d < 300) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let kept = self.paths.failed.join(format!("{sid}.wav"));
            if std::fs::rename(&path, &kept).is_err() {
                continue;
            }
            let kept_str = kept.to_string_lossy();
            let row = NewRow {
                created_ms: sid,
                duration_ms: duration.map(|d| d as i64),
                status: store::FAILED,
                error: Some("Recovered after crash"),
                audio_path: Some(&kept_str),
                ..Default::default()
            };
            match self.store.insert(&row) {
                Ok(id) => log::info!("recovered spool {sid} as row {id}"),
                Err(e) => {
                    log::error!("recover {sid}: {e}");
                    let _ = std::fs::rename(&kept, &path);
                }
            }
        }
        self.enforce_retention();
    }

    /// At most `KEEP_FILES` kept WAVs / `KEEP_BYTES`, oldest dropped first.
    fn enforce_retention(&mut self) {
        let Ok(kept) = self.store.kept_audio() else {
            return;
        };
        let size = |p: &str| std::fs::metadata(p).map_or(0, |m| m.len());
        let mut total: u64 = kept.iter().map(|(_, p)| size(p)).sum();
        let mut count = kept.len();
        for (id, path) in &kept {
            if count <= KEEP_FILES && total <= KEEP_BYTES {
                break;
            }
            total -= size(path);
            count -= 1;
            let _ = std::fs::remove_file(Path::new(path));
            let _ = self.store.clear_audio(*id);
        }
    }
}
