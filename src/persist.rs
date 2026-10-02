//! What a finished dictation leaves behind, and how it is made durable.
//!
//! `decide_commit` is the only place that turns a provider outcome plus the capture's terminal
//! state into a status, a stored model/error and an audio rule. `persist_dictation` applies
//! that decision to the database and the WAV in a crash-safe order, and `recover` reconciles
//! whatever a crash left between those steps. File operations go through `AudioFs` so tests
//! can fail them one at a time against a real temporary SQLite database.

use crate::capture::{AudioState, CaptureReport};
use crate::event::SessionId;
use crate::outcome::{Failure, TranscriptOutcome};
use crate::store::{self, NewRow, Store};
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Whether the result may be delivered (pasted) or is history-only.
#[derive(Clone, Debug, PartialEq)]
pub enum FinishDisposition {
    Deliver,
    /// Cancel, timeout, lock, sleep or quit: never auto-paste, always keep audio.
    SaveOnly(Failure),
}

#[derive(Debug, PartialEq)]
pub struct CommitDecision {
    pub status: &'static str,
    pub text: String,
    pub model: Option<&'static str>,
    pub error: Option<String>,
    /// Move the WAV to the kept folder (otherwise it may be deleted once the row is stored).
    pub keep_audio: bool,
    /// Paste with the "may be cut off" warning.
    pub incomplete_paste: bool,
    pub may_deliver: bool,
    /// A recognized, clean silence: no row, the recording is deleted.
    pub nothing_heard: bool,
}

fn capture_note(c: &CaptureReport) -> Option<String> {
    if let Some(p) = &c.problem {
        return Some(p.clone());
    }
    if c.dropped_chunks > 0 {
        return Some(format!("{} audio chunks dropped", c.dropped_chunks));
    }
    (c.audio == AudioState::Recoverable).then(|| "recording was not finalized".to_string())
}

fn join(reason: &Failure, note: Option<String>) -> String {
    match note {
        Some(n) => format!("{reason}; {n}"),
        None => reason.to_string(),
    }
}

/// The truth table for a new dictation (Retry has its own guarded update).
pub fn decide_commit(
    provider: &TranscriptOutcome,
    capture: &CaptureReport,
    disposition: &FinishDisposition,
) -> CommitDecision {
    let clean = capture.clean();
    let note = capture_note(capture);
    let save_only = match disposition {
        FinishDisposition::Deliver => None,
        FinishDisposition::SaveOnly(r) => Some(r),
    };
    let failed = |reason: &Failure| CommitDecision {
        status: store::FAILED,
        text: String::new(),
        model: None,
        error: Some(match save_only {
            Some(r) => r.to_string(),
            None => join(reason, note.clone()),
        }),
        keep_audio: true,
        incomplete_paste: false,
        may_deliver: false,
        nothing_heard: false,
    };
    match provider {
        TranscriptOutcome::Complete { text, model } => CommitDecision {
            status: if clean { store::OK } else { store::PROVISIONAL },
            text: text.as_str().to_string(),
            model: Some(model),
            error: if clean { None } else { note },
            keep_audio: !clean || save_only.is_some(),
            incomplete_paste: !clean,
            may_deliver: save_only.is_none(),
            nothing_heard: false,
        },
        TranscriptOutcome::Incomplete {
            text,
            model,
            reason,
        } if !text.is_empty() => CommitDecision {
            status: store::PROVISIONAL,
            text: text.clone(),
            model: Some(model),
            error: Some(join(reason, note)),
            keep_audio: true,
            incomplete_paste: reason.warns() || !clean,
            may_deliver: save_only.is_none(),
            nothing_heard: false,
        },
        TranscriptOutcome::Incomplete { reason, .. } | TranscriptOutcome::Failed { reason } => {
            failed(reason)
        }
        TranscriptOutcome::Empty { .. } => {
            if clean && save_only.is_none() {
                CommitDecision {
                    status: store::OK,
                    text: String::new(),
                    model: None,
                    error: None,
                    keep_audio: false,
                    incomplete_paste: false,
                    may_deliver: false,
                    nothing_heard: true,
                }
            } else {
                // Silence we cannot vouch for (capture trouble, or the user bailed out).
                failed(&Failure::Capture("no speech confirmed".into()))
            }
        }
    }
}

/// The narrow file operations persistence depends on.
pub trait AudioFs {
    fn exists(&self, p: &Path) -> io::Result<bool>;
    /// Never replaces an existing destination.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, p: &Path) -> io::Result<()>;
    fn len(&self, p: &Path) -> u64;
}

pub struct RealFs;

impl AudioFs for RealFs {
    fn exists(&self, p: &Path) -> io::Result<bool> {
        p.try_exists()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if to.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination exists",
            ));
        }
        std::fs::rename(from, to)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_file(p)
    }

    fn len(&self, p: &Path) -> u64 {
        std::fs::metadata(p).map_or(0, |m| m.len())
    }
}

/// Removal that treats "already gone" as done.
fn gone(fs: &dyn AudioFs, p: &Path) -> io::Result<()> {
    match fs.remove(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The app's two audio folders.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub spool: PathBuf,
    pub failed: PathBuf,
}

impl Dirs {
    pub fn spool_path(&self, id: SessionId) -> PathBuf {
        self.spool.join(format!("{}.wav", id.0))
    }

    pub fn failed_path(&self, id: SessionId) -> PathBuf {
        self.failed.join(format!("{}.wav", id.0))
    }

    /// Whether `p` is a file directly inside an audio folder (no `..`, no other location).
    pub fn owns(&self, p: &Path) -> bool {
        !p.components().any(|c| matches!(c, Component::ParentDir))
            && [&self.spool, &self.failed]
                .iter()
                .any(|d| p.parent() == Some(d.as_path()))
    }
}

pub struct Commit<'a> {
    pub session: SessionId,
    pub created_ms: i64,
    pub duration_ms: u64,
    pub decision: &'a CommitDecision,
    pub audio: AudioState,
}

#[derive(Debug, PartialEq)]
pub struct Persisted {
    pub row_id: Option<i64>,
    /// False when this session was already stored: nothing was changed or delivered again.
    pub fresh: bool,
    /// Where the audio is now, if any survives.
    pub audio: Option<PathBuf>,
}

/// Makes a decision durable: row first (it references the real spool file), then the move or
/// delete, and only then does the caller deliver. A failure after the insert leaves a state
/// `recover` understands; nothing is ever deleted while a row still needs it.
pub fn persist_dictation(
    store: &Store,
    fs: &dyn AudioFs,
    dirs: &Dirs,
    c: &Commit<'_>,
) -> rusqlite::Result<Persisted> {
    let spool = dirs.spool_path(c.session);
    let d = c.decision;
    let has_audio = c.audio != AudioState::Absent && fs.exists(&spool).unwrap_or(true);
    if d.nothing_heard {
        let mut audio = None;
        if has_audio {
            // Persist the disposition before removal so failed cleanup cannot be imported
            // as a failed dictation on restart.
            store.tombstone_audio(&spool)?;
            match gone(fs, &spool) {
                Ok(()) => store.clear_tombstone(&c.session.0.to_string())?,
                Err(e) => {
                    log::warn!("session {}: removing silent recording: {e}", c.session.0);
                    audio = Some(spool);
                }
            }
        }
        return Ok(Persisted {
            row_id: None,
            fresh: true,
            audio,
        });
    }
    let spool_str = spool.to_string_lossy();
    let new = NewRow {
        created_ms: c.created_ms,
        text: &d.text,
        duration_ms: i64::try_from(c.duration_ms).ok(),
        model: d.model,
        status: d.status,
        error: d.error.as_deref(),
        audio_path: has_audio.then_some(&*spool_str),
    };
    let (row, fresh) = store.insert_dictation_with_audio_rule(c.session, &new, d.keep_audio)?;
    if !fresh {
        return Ok(Persisted {
            row_id: Some(row),
            fresh: false,
            audio: None,
        });
    }
    let mut audio = has_audio.then(|| spool.clone());
    if has_audio && d.keep_audio {
        let dest = dirs.failed_path(c.session);
        match fs.exists(&dest) {
            Ok(false) => match fs.rename(&spool, &dest) {
                Ok(()) => {
                    // A failed update leaves the row on the old path; recovery relinks it.
                    if !matches!(store.set_audio_path(row, &spool, &dest), Ok(true)) {
                        log::warn!("session {}: audio path update failed", c.session.0);
                    }
                    audio = Some(dest);
                }
                Err(e) => log::warn!("session {}: keeping audio failed: {e}", c.session.0),
            },
            _ => log::warn!("session {}: kept-audio name is taken", c.session.0),
        }
    } else if has_audio {
        match gone(fs, &spool) {
            Ok(()) => {
                audio = None;
                if !matches!(store.clear_audio_if_matches(row, &spool), Ok(true)) {
                    log::warn!("session {}: audio path clear failed", c.session.0);
                }
            }
            Err(e) => log::warn!("session {}: removing audio failed: {e}", c.session.0),
        }
    }
    Ok(Persisted {
        row_id: Some(row),
        fresh: true,
        audio,
    })
}

#[derive(Debug, Default, PartialEq)]
pub struct RecoverReport {
    pub imported: u32,
    pub relinked: u32,
    pub cleaned: u32,
    pub conflicts: u32,
}

/// Numeric-named WAVs (`<digits>.wav`, within i64) found in the audio folders.
fn scan(dirs: &Dirs) -> BTreeMap<i64, (Option<PathBuf>, Option<PathBuf>)> {
    let mut found: BTreeMap<i64, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for (i, dir) in [&dirs.spool, &dirs.failed].into_iter().enumerate() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".wav"))
            else {
                continue;
            };
            let Ok(n) = stem.parse::<i64>() else { continue };
            if n <= 0 || n.to_string() != stem {
                continue;
            }
            let slot = found.entry(n).or_default();
            if i == 0 {
                slot.0 = Some(path);
            } else {
                slot.1 = Some(path);
            }
        }
    }
    found
}

/// The allocator's floor: nothing at or below this may be issued as a new session id.
pub fn highest_known_session(store: &Store, dirs: &Dirs) -> u64 {
    let rows = store.max_native_session().unwrap_or(0);
    let files = scan(dirs).keys().next_back().copied().unwrap_or(0);
    let tombs = store
        .tombstones()
        .unwrap_or_default()
        .iter()
        .filter_map(|(s, _)| s.parse::<i64>().ok())
        .max()
        .unwrap_or(0);
    rows.max(files).max(tombs).max(0) as u64
}

/// Deletes files whose rows were deleted, skipping `pinned` ones (a worker still reads them).
/// A tombstone is cleared only once every candidate file is confirmed gone.
pub fn cleanup_tombstones(store: &Store, fs: &dyn AudioFs, dirs: &Dirs, pinned: &HashSet<PathBuf>) {
    let Ok(tombs) = store.tombstones() else {
        return;
    };
    for (stem, path) in tombs {
        let path = PathBuf::from(path);
        let mut candidates = Vec::new();
        if dirs.owns(&path) {
            candidates.push(path.clone());
        } else {
            log::warn!("tombstone {stem}: path is outside the audio folders; not touching it");
        }
        if let Ok(n) = stem.parse::<i64>()
            && n > 0
            && n.to_string() == stem
        {
            for p in [
                dirs.spool_path(SessionId(n as u64)),
                dirs.failed_path(SessionId(n as u64)),
            ] {
                if !candidates.contains(&p) {
                    candidates.push(p);
                }
            }
        }
        if candidates.iter().any(|c| pinned.contains(c)) {
            continue;
        }
        let mut all_gone = true;
        for c in &candidates {
            if let Err(e) = gone(fs, c) {
                log::warn!("tombstone {stem}: removing {} failed: {e}", c.display());
                all_gone = false;
            }
        }
        if all_gone {
            let _ = store.clear_tombstone(&stem);
        }
    }
}

/// Reconciles rows, files and tombstones after a crash or at every start. Idempotent.
pub fn recover(store: &Store, fs: &dyn AudioFs, dirs: &Dirs) -> RecoverReport {
    let mut rep = RecoverReport::default();
    cleanup_tombstones(store, fs, dirs, &HashSet::new());
    let owed: HashSet<String> = store
        .tombstones()
        .unwrap_or_default()
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    for (n, (spool, failed)) in scan(dirs) {
        if owed.contains(&n.to_string()) {
            continue; // deleted on purpose; removal is still owed, never re-import
        }
        let id = SessionId(n as u64);
        match (spool, failed) {
            (Some(s), Some(f)) => {
                rep.conflicts += 1;
                log::warn!("recover {n}: audio exists in both folders; keeping both");
                let referenced = [&s, &f]
                    .iter()
                    .any(|p| matches!(store.row_for_audio(p), Ok(Some(_))));
                let native_pointing = store.native_row(id).ok().flatten().is_some_and(|r| {
                    r.audio_path
                        .is_some_and(|p| Path::new(&p) == s || Path::new(&p) == f)
                });
                if !referenced && !native_pointing {
                    reconcile(store, fs, dirs, id, &f, false, &mut rep);
                }
            }
            (Some(p), None) => reconcile(store, fs, dirs, id, &p, true, &mut rep),
            (None, Some(p)) => reconcile(store, fs, dirs, id, &p, false, &mut rep),
            (None, None) => {}
        }
    }
    // References to files that no longer exist (deleted before the path was cleared).
    for (id, path) in store.kept_audio().unwrap_or_default() {
        let path = PathBuf::from(path);
        if dirs.owns(&path) && matches!(fs.exists(&path), Ok(false)) {
            let _ = store.clear_audio_if_matches(id, &path);
        }
    }
    if rep != RecoverReport::default() {
        log::info!("recovery: {rep:?}");
    }
    rep
}

fn reconcile(
    store: &Store,
    fs: &dyn AudioFs,
    dirs: &Dirs,
    id: SessionId,
    path: &Path,
    in_spool: bool,
    rep: &mut RecoverReport,
) {
    if let Ok(Some(row)) = store.row_for_audio(path) {
        if !in_spool {
            return;
        }
        let keep = store.meta(&format!("audio-rule:{}", id.0));
        if row.status == store::OK && matches!(keep, Ok(None)) {
            // A clean result whose recording was due for deletion.
            if gone(fs, path).is_ok() {
                let _ = store.clear_audio_if_matches(row.id, path);
                rep.cleaned += 1;
            }
        } else {
            let dest = dirs.failed_path(id);
            if matches!(fs.exists(&dest), Ok(false)) && fs.rename(path, &dest).is_ok() {
                let _ = store.set_audio_path(row.id, path, &dest);
                rep.relinked += 1;
            }
        }
        return;
    }
    match store.native_row(id) {
        Ok(Some(row)) => match row.audio_path.map(PathBuf::from) {
            None => {
                if matches!(store.link_audio_if_unset(row.id, path), Ok(true)) {
                    rep.relinked += 1;
                }
            }
            Some(old) if old != path => {
                if old == dirs.spool_path(id) && !in_spool {
                    // Moved to the kept folder but the row never learned of it.
                    if matches!(store.set_audio_path(row.id, &old, path), Ok(true)) {
                        rep.relinked += 1;
                    }
                } else {
                    log::warn!("recover {}: row points elsewhere; leaving the file", id.0);
                }
            }
            Some(_) => {}
        },
        Ok(None) => orphan(store, fs, dirs, id, path, in_spool, rep),
        Err(e) => log::error!("recover {}: {e}", id.0),
    }
}

/// A WAV no row knows about: a crash before the row was written, or a legacy file.
fn orphan(
    store: &Store,
    fs: &dyn AudioFs,
    dirs: &Dirs,
    id: SessionId,
    path: &Path,
    in_spool: bool,
    rep: &mut RecoverReport,
) {
    let duration = match crate::audio::repair(path) {
        Ok(d) => d,
        Err(e) => {
            // Not evidence the recording is empty: keep it for a human.
            log::warn!("recover {}: cannot repair, keeping: {e}", id.0);
            return;
        }
    };
    if duration < 300 {
        if gone(fs, path).is_ok() {
            rep.cleaned += 1;
        }
        return;
    }
    let path_str = path.to_string_lossy();
    let row = NewRow {
        created_ms: id.0 as i64,
        duration_ms: Some(duration as i64),
        status: store::FAILED,
        error: Some("Recovered after crash"),
        audio_path: Some(&path_str),
        ..Default::default()
    };
    let (row_id, fresh) = match store.insert_dictation_once(id, &row) {
        Ok(r) => r,
        Err(e) => {
            log::error!("recover {}: {e}", id.0);
            return;
        }
    };
    if !fresh {
        return;
    }
    rep.imported += 1;
    if in_spool {
        let dest = dirs.failed_path(id);
        if matches!(fs.exists(&dest), Ok(false)) && fs.rename(path, &dest).is_ok() {
            let _ = store.set_audio_path(row_id, path, &dest);
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Retention {
    pub removed: u32,
    /// Files over the limit that could not go (pinned by a worker, or removal failed).
    pub excess: u32,
}

/// Keeps at most `max_files` / `max_bytes` of kept audio, oldest first, never touching
/// `pinned` files. Expired audio loses its reference; the history text stays.
pub fn enforce_retention(
    store: &Store,
    fs: &dyn AudioFs,
    max_files: usize,
    max_bytes: u64,
    pinned: &HashSet<PathBuf>,
) -> Retention {
    let mut out = Retention::default();
    let Ok(kept) = store.kept_audio() else {
        return out;
    };
    let mut total: u64 = kept.iter().map(|(_, p)| fs.len(Path::new(p))).sum();
    let mut count = kept.len();
    for (id, path) in kept {
        if count <= max_files && total <= max_bytes {
            break;
        }
        let path = PathBuf::from(path);
        if pinned.contains(&path) {
            out.excess += 1;
            continue;
        }
        let size = fs.len(&path);
        match gone(fs, &path) {
            Ok(()) => {
                let _ = store.clear_audio_if_matches(id, &path);
                total = total.saturating_sub(size);
                count -= 1;
                out.removed += 1;
            }
            Err(e) => {
                log::warn!("retention: removing audio failed: {e}");
                out.excess += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::WavSpool;
    use crate::outcome::NonEmptyText;
    use crate::store::tests::temp_store;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct FaultFs {
        fail_rename: AtomicBool,
        fail_remove: AtomicBool,
    }

    impl AudioFs for FaultFs {
        fn exists(&self, p: &Path) -> io::Result<bool> {
            RealFs.exists(p)
        }
        fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
            if self.fail_rename.load(Ordering::SeqCst) {
                return Err(io::Error::other("injected rename failure"));
            }
            RealFs.rename(a, b)
        }
        fn remove(&self, p: &Path) -> io::Result<()> {
            if self.fail_remove.load(Ordering::SeqCst) {
                return Err(io::Error::other("injected remove failure"));
            }
            RealFs.remove(p)
        }
        fn len(&self, p: &Path) -> u64 {
            RealFs.len(p)
        }
    }

    struct Env {
        store: Store,
        dirs: Dirs,
    }

    fn env() -> Env {
        let (store, dir) = temp_store();
        let dirs = Dirs {
            spool: dir.join("spool"),
            failed: dir.join("failed"),
        };
        std::fs::create_dir_all(&dirs.spool).unwrap();
        std::fs::create_dir_all(&dirs.failed).unwrap();
        Env { store, dirs }
    }

    /// A finalized synthetic WAV of `ms` milliseconds.
    fn wav(path: &Path, ms: usize) {
        let mut w = WavSpool::create(path).unwrap();
        w.write(&vec![100i16; ms * 16]).unwrap();
        w.finish().unwrap();
    }

    fn rows(e: &Env) -> Vec<store::Row> {
        e.store.search("", 100).unwrap()
    }

    fn complete(text: &str) -> TranscriptOutcome {
        TranscriptOutcome::Complete {
            text: NonEmptyText::new(text).unwrap(),
            model: "m",
        }
    }

    fn clean() -> CaptureReport {
        CaptureReport {
            duration_ms: 1000,
            dropped_chunks: 0,
            problem: None,
            audio: AudioState::Finalized,
        }
    }

    fn commit(e: &Env, id: u64, d: &CommitDecision, audio: AudioState) -> Persisted {
        persist_dictation(
            &e.store,
            &RealFs,
            &e.dirs,
            &Commit {
                session: SessionId(id),
                created_ms: id as i64,
                duration_ms: 1000,
                decision: d,
                audio,
            },
        )
        .unwrap()
    }

    fn keep_decision() -> CommitDecision {
        decide_commit(
            &TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
            &clean(),
            &FinishDisposition::Deliver,
        )
    }

    // ---- decide_commit truth table ----

    #[test]
    fn complete_and_clean_is_ok_and_audio_is_disposable() {
        let d = decide_commit(&complete("hi"), &clean(), &FinishDisposition::Deliver);
        assert_eq!(
            (d.status, d.keep_audio, d.incomplete_paste),
            (store::OK, false, false)
        );
        assert!(d.may_deliver && d.error.is_none());
    }

    #[test]
    fn complete_with_capture_trouble_is_provisional_and_keeps_audio() {
        for report in [
            CaptureReport {
                dropped_chunks: 3,
                ..clean()
            },
            CaptureReport {
                problem: Some("device lost".into()),
                ..clean()
            },
            CaptureReport {
                audio: AudioState::Recoverable,
                ..clean()
            },
        ] {
            let d = decide_commit(&complete("hi"), &report, &FinishDisposition::Deliver);
            assert_eq!(d.status, store::PROVISIONAL);
            assert!(d.keep_audio && d.incomplete_paste && d.may_deliver);
            assert!(d.error.is_some());
        }
    }

    #[test]
    fn incomplete_text_is_provisional_and_warns_only_for_real_problems() {
        let inc = |reason| TranscriptOutcome::Incomplete {
            text: "partial".into(),
            model: "live",
            reason,
        };
        let d = decide_commit(
            &inc(Failure::Timeout),
            &clean(),
            &FinishDisposition::Deliver,
        );
        assert_eq!(d.status, store::PROVISIONAL);
        assert!(d.keep_audio && d.incomplete_paste && d.may_deliver);
        assert_eq!(d.model, Some("live"));
        // Unconfirmed Live: recorded and kept, but not a warning on every dictation.
        let d = decide_commit(
            &inc(Failure::Unconfirmed),
            &clean(),
            &FinishDisposition::Deliver,
        );
        assert_eq!(d.status, store::PROVISIONAL);
        assert!(d.keep_audio && !d.incomplete_paste);
        // ...unless the capture itself had trouble.
        let bad = CaptureReport {
            dropped_chunks: 1,
            ..clean()
        };
        assert!(
            decide_commit(
                &inc(Failure::Unconfirmed),
                &bad,
                &FinishDisposition::Deliver
            )
            .incomplete_paste
        );
    }

    #[test]
    fn failures_and_blank_partials_store_nothing_to_paste() {
        let d = keep_decision();
        assert_eq!(
            (d.status, d.keep_audio, d.may_deliver),
            (store::FAILED, true, false)
        );
        assert!(d.text.is_empty() && d.error.is_some());
        let blank = TranscriptOutcome::Incomplete {
            text: String::new(),
            model: "live",
            reason: Failure::Unconfirmed,
        };
        let d = decide_commit(&blank, &clean(), &FinishDisposition::Deliver);
        assert_eq!((d.status, d.may_deliver), (store::FAILED, false));
    }

    #[test]
    fn recognized_empty_is_nothing_heard_only_when_capture_is_clean() {
        let empty = TranscriptOutcome::Empty { model: "b" };
        let d = decide_commit(&empty, &clean(), &FinishDisposition::Deliver);
        assert!(d.nothing_heard && !d.keep_audio);
        let bad = CaptureReport {
            dropped_chunks: 2,
            ..clean()
        };
        let d = decide_commit(&empty, &bad, &FinishDisposition::Deliver);
        assert!(!d.nothing_heard);
        assert_eq!((d.status, d.keep_audio), (store::FAILED, true));
        let d = decide_commit(
            &empty,
            &clean(),
            &FinishDisposition::SaveOnly(Failure::Cancelled),
        );
        assert!(!d.nothing_heard && d.keep_audio);
    }

    #[test]
    fn save_only_never_delivers_but_keeps_late_text_and_audio() {
        let s = FinishDisposition::SaveOnly(Failure::Interrupted);
        let d = decide_commit(&complete("late"), &clean(), &s);
        assert_eq!(d.text, "late");
        assert!(!d.may_deliver && d.keep_audio);
        let d = decide_commit(
            &TranscriptOutcome::Failed {
                reason: Failure::Timeout,
            },
            &clean(),
            &s,
        );
        assert!(!d.may_deliver && d.keep_audio);
        assert!(d.error.unwrap().contains("Interrupted"));
    }

    // ---- persistence ordering ----

    #[test]
    fn clean_result_is_stored_then_audio_deleted() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(10));
        wav(&spool, 500);
        let d = decide_commit(&complete("hello"), &clean(), &FinishDisposition::Deliver);
        let p = commit(&e, 10, &d, AudioState::Finalized);
        assert!(p.fresh && p.audio.is_none() && !spool.exists());
        let r = &rows(&e)[0];
        assert_eq!((r.text.as_str(), r.audio_path.clone()), ("hello", None));
    }

    #[test]
    fn kept_audio_moves_and_the_row_follows() {
        let e = env();
        wav(&e.dirs.spool_path(SessionId(11)), 500);
        let p = commit(&e, 11, &keep_decision(), AudioState::Finalized);
        let dest = e.dirs.failed_path(SessionId(11));
        assert_eq!(p.audio.as_deref(), Some(dest.as_path()));
        assert!(dest.exists() && !e.dirs.spool_path(SessionId(11)).exists());
        assert_eq!(
            rows(&e)[0].audio_path.as_deref(),
            Some(dest.to_str().unwrap())
        );
    }

    #[test]
    fn nothing_heard_deletes_audio_and_stores_no_row() {
        let e = env();
        wav(&e.dirs.spool_path(SessionId(12)), 500);
        let d = decide_commit(
            &TranscriptOutcome::Empty { model: "b" },
            &clean(),
            &FinishDisposition::Deliver,
        );
        let p = commit(&e, 12, &d, AudioState::Finalized);
        assert_eq!(p.row_id, None);
        assert!(!e.dirs.spool_path(SessionId(12)).exists());
        assert!(rows(&e).is_empty());
    }

    // ---- persistence fault suite (V2 items 1-15) ----

    /// 1. A valid orphan is repaired and becomes exactly one failed row, twice over.
    #[test]
    fn crash_before_insert_recovers_one_row_idempotently() {
        let e = env();
        wav(&e.dirs.spool_path(SessionId(1000)), 800);
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        let r = rows(&e);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, store::FAILED);
        assert!(e.dirs.failed_path(SessionId(1000)).exists());
        assert!(!e.dirs.spool_path(SessionId(1000)).exists());
        assert_eq!(
            r[0].audio_path.as_deref(),
            Some(e.dirs.failed_path(SessionId(1000)).to_str().unwrap())
        );
    }

    /// 2. Row inserted pointing at the spool file, crash before the rename.
    #[test]
    fn crash_after_insert_before_move_keeps_one_row() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(20));
        wav(&spool, 500);
        let s = spool.to_string_lossy();
        e.store
            .insert_dictation_once(
                SessionId(20),
                &NewRow {
                    status: store::PROVISIONAL,
                    text: "partial",
                    audio_path: Some(&s),
                    ..Default::default()
                },
            )
            .unwrap();
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        let r = rows(&e);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "partial");
        let kept = e.dirs.failed_path(SessionId(20));
        assert!(kept.exists());
        assert_eq!(r[0].audio_path.as_deref(), Some(kept.to_str().unwrap()));
    }

    /// 3. The rename happened but the path update did not.
    #[test]
    fn moved_file_with_stale_row_path_is_relinked() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(30));
        let kept = e.dirs.failed_path(SessionId(30));
        wav(&kept, 500);
        let s = spool.to_string_lossy();
        e.store
            .insert_dictation_once(
                SessionId(30),
                &NewRow {
                    status: store::FAILED,
                    audio_path: Some(&s),
                    ..Default::default()
                },
            )
            .unwrap();
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        assert_eq!(rows(&e).len(), 1);
        assert_eq!(
            rows(&e)[0].audio_path.as_deref(),
            Some(kept.to_str().unwrap())
        );
    }

    /// 4. A failing rename leaves the row on the real spool path, not a made-up one.
    #[test]
    fn failed_rename_keeps_the_spool_reference() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(40));
        wav(&spool, 500);
        let fs = FaultFs::default();
        fs.fail_rename.store(true, Ordering::SeqCst);
        let p = persist_dictation(
            &e.store,
            &fs,
            &e.dirs,
            &Commit {
                session: SessionId(40),
                created_ms: 40,
                duration_ms: 500,
                decision: &keep_decision(),
                audio: AudioState::Finalized,
            },
        )
        .unwrap();
        assert_eq!(p.audio.as_deref(), Some(spool.as_path()));
        assert!(spool.exists());
        assert_eq!(
            rows(&e)[0].audio_path.as_deref(),
            Some(spool.to_str().unwrap())
        );
        // Recovery then finishes the move once the filesystem cooperates.
        recover(&e.store, &RealFs, &e.dirs);
        assert!(e.dirs.failed_path(SessionId(40)).exists());
        assert_eq!(rows(&e).len(), 1);
    }

    /// 5. A clean row exists but the delete never happened.
    #[test]
    fn clean_row_with_leftover_audio_is_cleaned_up_without_duplicates() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(50));
        wav(&spool, 500);
        let fs = FaultFs::default();
        fs.fail_remove.store(true, Ordering::SeqCst);
        let d = decide_commit(&complete("ok"), &clean(), &FinishDisposition::Deliver);
        let p = persist_dictation(
            &e.store,
            &fs,
            &e.dirs,
            &Commit {
                session: SessionId(50),
                created_ms: 50,
                duration_ms: 500,
                decision: &d,
                audio: AudioState::Finalized,
            },
        )
        .unwrap();
        assert!(spool.exists(), "failed delete leaves the file");
        assert_eq!(p.audio.as_deref(), Some(spool.as_path()));
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        assert!(!spool.exists());
        let r = rows(&e);
        assert_eq!((r.len(), r[0].audio_path.clone()), (1, None));
    }

    /// 6. The file was deleted but the reference was not cleared.
    #[test]
    fn dangling_reference_is_cleared_without_a_recovery_row() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(60));
        let s = spool.to_string_lossy();
        e.store
            .insert_dictation_once(
                SessionId(60),
                &NewRow {
                    status: store::OK,
                    text: "t",
                    audio_path: Some(&s),
                    ..Default::default()
                },
            )
            .unwrap();
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        let r = rows(&e);
        assert_eq!((r.len(), r[0].audio_path.clone()), (1, None));
    }

    /// 7. If the database insert fails the audio stays and nothing is reported durable.
    #[test]
    fn insert_failure_preserves_audio_and_reports_error() {
        let e = env();
        let spool = e.dirs.spool_path(SessionId(u64::MAX));
        wav(&spool, 500);
        let r = persist_dictation(
            &e.store,
            &RealFs,
            &e.dirs,
            &Commit {
                session: SessionId(u64::MAX),
                created_ms: 1,
                duration_ms: 500,
                decision: &keep_decision(),
                audio: AudioState::Finalized,
            },
        );
        assert!(r.is_err());
        assert!(spool.exists());
        assert!(rows(&e).is_empty());
    }

    /// 8. A legacy row (no native identity) that references a file is not imported again.
    #[test]
    fn legacy_referenced_audio_is_not_duplicated() {
        let e = env();
        let kept = e.dirs.failed_path(SessionId(80));
        wav(&kept, 500);
        let s = kept.to_string_lossy();
        e.store
            .insert(&NewRow {
                status: store::FAILED,
                audio_path: Some(&s),
                created_ms: 80,
                ..Default::default()
            })
            .unwrap();
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        assert_eq!(rows(&e).len(), 1);
        assert!(kept.exists());
    }

    /// 9. An unreferenced file in the kept folder becomes one recovered row.
    #[test]
    fn legacy_unreferenced_failed_file_becomes_one_row() {
        let e = env();
        let kept = e.dirs.failed_path(SessionId(90));
        wav(&kept, 700);
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        let r = rows(&e);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].audio_path.as_deref(), Some(kept.to_str().unwrap()));
    }

    /// 10. The same id in both folders: neither is overwritten or deleted.
    #[test]
    fn both_folders_conflict_preserves_both_files() {
        let e = env();
        let (s, f) = (
            e.dirs.spool_path(SessionId(100)),
            e.dirs.failed_path(SessionId(100)),
        );
        wav(&s, 500);
        wav(&f, 700);
        let before = (std::fs::read(&s).unwrap(), std::fs::read(&f).unwrap());
        for _ in 0..2 {
            let rep = recover(&e.store, &RealFs, &e.dirs);
            assert!(rep.conflicts >= 1);
        }
        assert!(s.exists() && f.exists());
        assert_eq!(
            before,
            (std::fs::read(&s).unwrap(), std::fs::read(&f).unwrap())
        );
        assert_eq!(rows(&e).len(), 1, "one recovered row, for the kept file");
    }

    /// 11. A file that cannot be repaired is not evidence of silence: keep it.
    #[test]
    fn unrepairable_file_is_kept_not_deleted() {
        let e = env();
        let p = e.dirs.spool_path(SessionId(110));
        std::fs::write(&p, b"RIFF-truncated").unwrap();
        for _ in 0..2 {
            recover(&e.store, &RealFs, &e.dirs);
        }
        assert!(p.exists());
        assert!(rows(&e).is_empty());
    }

    #[test]
    fn short_recordings_are_cleaned_up() {
        let e = env();
        let p = e.dirs.spool_path(SessionId(111));
        wav(&p, 100);
        recover(&e.store, &RealFs, &e.dirs);
        assert!(!p.exists());
        assert!(rows(&e).is_empty());
    }

    /// 12. Deleting a row tombstones its audio; a pinned file survives until released, and
    ///     a restart never resurrects it.
    #[test]
    fn delete_while_pinned_defers_removal_and_blocks_resurrection() {
        let mut e = env();
        let kept = e.dirs.failed_path(SessionId(120));
        wav(&kept, 500);
        let s = kept.to_string_lossy();
        let (id, _) = e
            .store
            .insert_dictation_once(
                SessionId(120),
                &NewRow {
                    status: store::FAILED,
                    audio_path: Some(&s),
                    ..Default::default()
                },
            )
            .unwrap();
        e.store.delete(id).unwrap();
        let pinned: HashSet<PathBuf> = [kept.clone()].into();
        cleanup_tombstones(&e.store, &RealFs, &e.dirs, &pinned);
        assert!(kept.exists(), "pinned file is not deleted");
        // A restart in this state must not turn the file back into a dictation.
        recover(&e.store, &RealFs, &e.dirs);
        assert!(rows(&e).is_empty());
        cleanup_tombstones(&e.store, &RealFs, &e.dirs, &HashSet::new());
        assert!(!kept.exists());
        assert!(e.store.tombstones().unwrap().is_empty());
    }

    /// 13. A failing delete keeps the tombstone; success (or NotFound) clears it.
    #[test]
    fn tombstone_survives_failed_delete_and_clears_when_file_is_gone() {
        let mut e = env();
        let kept = e.dirs.failed_path(SessionId(130));
        wav(&kept, 500);
        let s = kept.to_string_lossy();
        let (id, _) = e
            .store
            .insert_dictation_once(
                SessionId(130),
                &NewRow {
                    status: store::FAILED,
                    audio_path: Some(&s),
                    ..Default::default()
                },
            )
            .unwrap();
        e.store.delete(id).unwrap();
        let fs = FaultFs::default();
        fs.fail_remove.store(true, Ordering::SeqCst);
        cleanup_tombstones(&e.store, &fs, &e.dirs, &HashSet::new());
        assert_eq!(e.store.tombstones().unwrap().len(), 1);
        recover(&e.store, &fs, &e.dirs);
        assert!(rows(&e).is_empty(), "no resurrection while removal fails");
        fs.fail_remove.store(false, Ordering::SeqCst);
        cleanup_tombstones(&e.store, &fs, &e.dirs, &HashSet::new());
        assert!(e.store.tombstones().unwrap().is_empty());
        // Already-absent file: cleared idempotently.
        e.store
            .set_meta(
                "tomb:131",
                e.dirs.failed_path(SessionId(131)).to_str().unwrap(),
            )
            .unwrap();
        cleanup_tombstones(&e.store, &fs, &e.dirs, &HashSet::new());
        assert!(e.store.tombstones().unwrap().is_empty());
    }

    #[test]
    fn tombstones_never_delete_outside_the_audio_folders() {
        let e = env();
        let outside = e.dirs.spool.parent().unwrap().join("precious.txt");
        std::fs::write(&outside, b"keep me").unwrap();
        e.store
            .set_meta("tomb:precious", outside.to_str().unwrap())
            .unwrap();
        let sneaky = e.dirs.spool.join("..").join("precious.txt");
        e.store
            .set_meta("tomb:sneaky", sneaky.to_str().unwrap())
            .unwrap();
        cleanup_tombstones(&e.store, &RealFs, &e.dirs, &HashSet::new());
        assert!(outside.exists());
        assert!(!e.dirs.owns(&outside) && !e.dirs.owns(&sneaky));
    }

    /// 14. Retention skips pins and reruns when they are released; expiry keeps history.
    #[test]
    fn retention_skips_pins_and_keeps_text() {
        let e = env();
        let mut paths = Vec::new();
        for n in 1..=4u64 {
            let p = e.dirs.failed_path(SessionId(1000 + n));
            wav(&p, 200);
            let s = p.to_string_lossy();
            e.store
                .insert_dictation_once(
                    SessionId(1000 + n),
                    &NewRow {
                        created_ms: n as i64,
                        text: "words",
                        status: store::PROVISIONAL,
                        audio_path: Some(&s),
                        ..Default::default()
                    },
                )
                .unwrap();
            paths.push(p);
        }
        let pinned: HashSet<PathBuf> = [paths[0].clone()].into();
        let r = enforce_retention(&e.store, &RealFs, 2, u64::MAX, &pinned);
        assert_eq!(
            r,
            Retention {
                removed: 2,
                excess: 1
            }
        );
        assert!(paths[0].exists() && !paths[1].exists() && !paths[2].exists() && paths[3].exists());
        assert_eq!(rows(&e).len(), 4, "history text stays");
        assert_eq!(
            rows(&e).iter().filter(|r| r.audio_path.is_some()).count(),
            2
        );
        let r = enforce_retention(&e.store, &RealFs, 2, u64::MAX, &HashSet::new());
        assert_eq!(r.removed, 0);
        let r = enforce_retention(&e.store, &RealFs, 1, u64::MAX, &HashSet::new());
        assert_eq!(r.removed, 1);
        assert!(!paths[0].exists());
    }

    /// 15. Repeated terminal events: one row, one disposition.
    #[test]
    fn repeated_completion_is_a_no_op() {
        let e = env();
        wav(&e.dirs.spool_path(SessionId(150)), 500);
        let d = decide_commit(&complete("once"), &clean(), &FinishDisposition::Deliver);
        let a = commit(&e, 150, &d, AudioState::Finalized);
        let b = commit(&e, 150, &keep_decision(), AudioState::Finalized);
        assert!(a.fresh && !b.fresh);
        assert_eq!(a.row_id, b.row_id);
        let r = rows(&e);
        assert_eq!(
            (r.len(), r[0].text.as_str(), r[0].status.as_str()),
            (1, "once", store::OK)
        );
    }

    #[test]
    fn allocator_floor_covers_rows_files_and_tombstones() {
        let e = env();
        assert_eq!(highest_known_session(&e.store, &e.dirs), 0);
        e.store
            .insert_dictation_once(
                SessionId(500),
                &NewRow {
                    status: store::OK,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(highest_known_session(&e.store, &e.dirs), 500);
        wav(&e.dirs.spool_path(SessionId(900)), 100);
        assert_eq!(highest_known_session(&e.store, &e.dirs), 900);
        e.store.set_meta("tomb:1200", "x").unwrap();
        assert_eq!(highest_known_session(&e.store, &e.dirs), 1200);
        std::fs::write(e.dirs.spool.join("abc.wav"), b"x").unwrap();
        std::fs::write(e.dirs.spool.join("007.wav"), b"x").unwrap();
        assert_eq!(
            highest_known_session(&e.store, &e.dirs),
            1200,
            "non-canonical names ignored"
        );
    }
    #[test]
    fn review_save_only_complete_rename_failure_survives_restart() {
        let e = env();
        let id = SessionId(900001);
        let spool = e.dirs.spool_path(id);
        wav(&spool, 1000);
        let d = decide_commit(
            &complete("saved text"),
            &clean(),
            &FinishDisposition::SaveOnly(Failure::Interrupted),
        );
        assert!(d.keep_audio);
        let fs = FaultFs::default();
        fs.fail_rename.store(true, Ordering::SeqCst);
        persist_dictation(
            &e.store,
            &fs,
            &e.dirs,
            &Commit {
                session: id,
                created_ms: id.0 as i64,
                duration_ms: 1000,
                decision: &d,
                audio: AudioState::Finalized,
            },
        )
        .unwrap();
        assert!(spool.exists());
        recover(&e.store, &RealFs, &e.dirs);
        assert!(
            spool.exists() || e.dirs.failed_path(id).exists(),
            "SaveOnly audio must survive restart after rename failure"
        );
    }

    #[test]
    fn review_silent_remove_failure_reports_retained_audio() {
        let e = env();
        let id = SessionId(900002);
        let spool = e.dirs.spool_path(id);
        wav(&spool, 1000);
        let d = decide_commit(
            &TranscriptOutcome::Empty { model: "m" },
            &clean(),
            &FinishDisposition::Deliver,
        );
        let fs = FaultFs::default();
        fs.fail_remove.store(true, Ordering::SeqCst);
        let result = persist_dictation(
            &e.store,
            &fs,
            &e.dirs,
            &Commit {
                session: id,
                created_ms: id.0 as i64,
                duration_ms: 1000,
                decision: &d,
                audio: AudioState::Finalized,
            },
        )
        .unwrap();
        assert!(spool.exists());
        assert!(
            result.audio.is_some(),
            "Failed removal must not claim no retained audio"
        );
    }
}
