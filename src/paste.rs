//! Paste worker: one job at a time, after the history row is committed.
//!
//! Delivery is only ever to the window dictation stopped in, proven by handle *and* process and
//! thread, and only while the session's `DeliveryPermission` still holds. Anything else becomes
//! a copy to the (private) clipboard, or nothing at all when delivery was withdrawn. The desktop
//! is behind `Desktop` so the decision order is testable without touching the real clipboard.

use crate::event::{ActionFailure, Event, RequestId, SessionId};
use crate::win::clipboard::{self, ClipboardError, RestoreOutcome};
use crate::win::input::{self, Sent};
use crate::win::overlay::Tone;
use crate::win::window::{self, Reach, Window};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use windows::Win32::Foundation::HWND;

const TERMINAL_CLASSES: &[&str] = &[
    "ConsoleWindowClass",
    "CASCADIA_HOSTING_WINDOW_CLASS",
    "mintty",
    "VirtualConsoleClass",
    "PuTTY",
    "Alacritty",
    "org.wezfurlong.wezterm",
    "Hyper",
    "TMobaXterm",
    "kitty",
];
/// Electron terminals share Chrome's window class, so they're matched by exe.
const TERMINAL_EXES: &[&str] = &["termius.exe", "tabby.exe", "wave.exe", "rio.exe"];

/// Wait before restoring the clipboard, so slow targets have read it (OpenWhispr's value).
const RESTORE_DELAY: Duration = Duration::from_millis(500);

/// Lock, sleep and quit close this for every session at once; unlock and resume only reopen it
/// for sessions that start afterwards (a permission remembers the generation it was issued in).
#[derive(Debug, Default)]
pub struct DeliveryGate {
    generation: AtomicU64,
    blocked: AtomicBool,
}

impl DeliveryGate {
    pub fn new() -> Arc<DeliveryGate> {
        Arc::new(DeliveryGate::default())
    }

    /// Withdraws every permission issued so far and blocks new deliveries.
    pub fn block(&self) {
        self.blocked.store(true, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Reopens the gate. Permissions withdrawn by `block` stay withdrawn.
    pub fn unblock(&self) {
        self.blocked.store(false, Ordering::SeqCst);
    }

    pub fn permit(self: &Arc<Self>) -> Arc<DeliveryPermission> {
        Arc::new(DeliveryPermission {
            revoked: AtomicBool::new(false),
            gate: self.clone(),
            gate_gen: self.generation.load(Ordering::SeqCst),
        })
    }
}

/// May this one dictation still be pasted? Checked by the paste worker immediately before it
/// touches the clipboard and again before it sends keys.
#[derive(Debug)]
pub struct DeliveryPermission {
    revoked: AtomicBool,
    gate: Arc<DeliveryGate>,
    gate_gen: u64,
}

impl DeliveryPermission {
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    pub fn allowed(&self) -> bool {
        !self.revoked.load(Ordering::SeqCst)
            && self.gate.generation.load(Ordering::SeqCst) == self.gate_gen
            && !self.gate.blocked.load(Ordering::SeqCst)
    }
}

/// The window dictation stopped in, with enough identity to notice that the handle has been
/// recycled by another process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteTarget {
    pub hwnd: Window,
    pub process_id: u32,
    pub thread_id: u32,
}

impl PasteTarget {
    pub fn capture(hwnd: Window) -> PasteTarget {
        let (process_id, thread_id) = window::pid_and_thread(hwnd);
        PasteTarget {
            hwnd,
            process_id,
            thread_id,
        }
    }
}

pub enum Job {
    Paste(PasteRequest),
    /// Copy from history: set the clipboard (a deliberate copy, so history may keep it).
    Copy {
        request: RequestId,
        text: String,
    },
}

pub struct PasteRequest {
    pub session: SessionId,
    pub row_id: i64,
    pub text: String,
    pub target: Option<PasteTarget>,
    /// The text may be cut off.
    pub incomplete: bool,
    pub permission: Arc<DeliveryPermission>,
}

/// Stored in `transcriptions.paste`.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The keystroke was sent in full. Whether the target accepted it is not observable.
    Attempted,
    /// Not pasted, but left on the clipboard (out of clipboard history).
    Copied(&'static str),
    Skipped(&'static str),
    Failed(&'static str),
}

impl Outcome {
    pub fn db_value(&self) -> String {
        match self {
            Outcome::Attempted => "attempted".into(),
            Outcome::Copied(r) => format!("copied:{r}"),
            Outcome::Skipped(r) => format!("skipped:{r}"),
            Outcome::Failed(r) => format!("failed:{r}"),
        }
    }

    /// Notice for the overlay unless the text simply went in.
    pub fn notice(&self, incomplete: bool) -> Option<(&'static str, Tone)> {
        Some(match self {
            Outcome::Attempted if incomplete => (
                "Pasted, but it may be cut off — Retry is in history",
                Tone::Error,
            ),
            Outcome::Attempted => return None,
            Outcome::Copied("elevated") => {
                ("Copied — can't paste into an admin window", Tone::Info)
            }
            Outcome::Copied(_) => ("Copied — paste it where you need it", Tone::Info),
            Outcome::Skipped("withheld") => ("Saved to history — not pasted", Tone::Info),
            Outcome::Skipped(_) => ("Saved to history — no text box to paste into", Tone::Error),
            Outcome::Failed("input-blocked" | "input-partial") => {
                ("Couldn't paste — text is on the clipboard", Tone::Error)
            }
            Outcome::Failed(_) => ("Couldn't paste — text is in history", Tone::Error),
        })
    }
}

pub struct Pasted {
    pub session: SessionId,
    pub row_id: i64,
    pub incomplete: bool,
    pub outcome: Outcome,
}

/// Everything delivery needs from the desktop.
pub trait Desktop {
    type Lease;
    fn window_exists(&self, w: Window) -> bool;
    fn identity(&self, w: Window) -> (u32, u32);
    fn foreground(&self) -> Option<Window>;
    fn focus(&self, w: Window) -> bool;
    fn is_own(&self, w: Window) -> bool;
    fn reach(&self, w: Window) -> Reach;
    fn is_terminal(&self, w: Window) -> bool;
    fn begin_paste(&self, text: &str) -> Result<Self::Lease, ClipboardError>;
    fn lease_owned(&self, lease: &Self::Lease) -> bool;
    fn send_paste(&self, terminal: bool) -> Sent;
    fn restore(&self, lease: &Self::Lease) -> RestoreOutcome;
    /// A private copy (kept out of clipboard history).
    fn copy_private(&self, text: &str) -> Result<(), ClipboardError>;
    fn pause(&self, d: Duration);
}

struct Native {
    owner: HWND,
}

impl Desktop for Native {
    type Lease = clipboard::ClipboardLease;

    fn window_exists(&self, w: Window) -> bool {
        window::exists(w)
    }
    fn identity(&self, w: Window) -> (u32, u32) {
        window::pid_and_thread(w)
    }
    fn foreground(&self) -> Option<Window> {
        window::foreground()
    }
    fn focus(&self, w: Window) -> bool {
        window::focus(w)
    }
    fn is_own(&self, w: Window) -> bool {
        window::is_own(w)
    }
    fn reach(&self, w: Window) -> Reach {
        window::reach(w)
    }
    fn is_terminal(&self, w: Window) -> bool {
        let class = window::class_name(w);
        TERMINAL_CLASSES
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&class))
            || window::exe_name(w).is_some_and(|exe| TERMINAL_EXES.contains(&exe.as_str()))
    }
    fn begin_paste(&self, text: &str) -> Result<Self::Lease, ClipboardError> {
        clipboard::begin_paste(self.owner, text)
    }
    fn lease_owned(&self, lease: &Self::Lease) -> bool {
        clipboard::still_owned(lease)
    }
    fn send_paste(&self, terminal: bool) -> Sent {
        input::paste(terminal)
    }
    fn restore(&self, lease: &Self::Lease) -> RestoreOutcome {
        clipboard::restore_if_unchanged(self.owner, lease)
    }
    fn copy_private(&self, text: &str) -> Result<(), ClipboardError> {
        clipboard::copy_text(self.owner, text, true)
    }
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Starts the paste thread. Results come back as `Event::Pasted` / `Event::Copied`. `Err`
/// means there is no worker, so nothing will answer.
pub fn spawn(events: Sender<Event>) -> io::Result<Sender<Job>> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("paste".into())
        .spawn(move || run(rx, events))?;
    Ok(tx)
}

fn run(jobs: Receiver<Job>, events: Sender<Event>) {
    let owner = window::message_window()
        .map_err(|e| log::error!("paste: no clipboard window: {e}"))
        .ok();
    for job in jobs {
        match job {
            Job::Paste(req) => {
                let outcome = match owner {
                    Some(owner) => deliver(&Native { owner }, &req),
                    None => Outcome::Failed("clipboard"),
                };
                log::info!("paste row {}: {:?}", req.row_id, outcome);
                let _ = events.send(Event::Pasted(Pasted {
                    session: req.session,
                    row_id: req.row_id,
                    incomplete: req.incomplete,
                    outcome,
                }));
            }
            Job::Copy { request, text } => {
                // Success is reported only after the clipboard really took the text.
                let result = match owner {
                    Some(owner) => clipboard::copy_text(owner, &text, false).map_err(|e| {
                        log::warn!("copy: {e:?}");
                        ActionFailure(
                            match e {
                                ClipboardError::Busy => "The clipboard is busy",
                                _ => "Couldn't copy to the clipboard",
                            }
                            .into(),
                        )
                    }),
                    None => Err(ActionFailure("Couldn't copy to the clipboard".into())),
                };
                let _ = events.send(Event::Copied { request, result });
            }
        }
    }
}

/// No text box to paste into: leave the text on the clipboard instead (still private).
fn copy_instead<D: Desktop>(d: &D, req: &PasteRequest, why: &'static str) -> Outcome {
    if !req.permission.allowed() {
        return Outcome::Skipped("withheld");
    }
    match d.copy_private(&req.text) {
        Ok(()) => Outcome::Copied(why),
        Err(_) => Outcome::Skipped(why),
    }
}

/// The paste decision. Nothing here runs unless the permission still holds, and the foreground
/// window, permission and clipboard ownership are all checked again right before the keys.
pub fn deliver<D: Desktop>(d: &D, req: &PasteRequest) -> Outcome {
    let withheld = || Outcome::Skipped("withheld");
    if !req.permission.allowed() {
        return withheld();
    }
    // Paste only into the window dictation stopped in. If there was none, or it is gone, was
    // recycled, or won't come back to the front, the user has moved on: never drop the text
    // into whatever happens to be in front now.
    let Some(target) = req.target else {
        return copy_instead(d, req, "no-window");
    };
    let t = target.hwnd;
    if !d.window_exists(t) || d.identity(t) != (target.process_id, target.thread_id) {
        return copy_instead(d, req, "window-closed");
    }
    let was_front = d.foreground() == Some(t);
    if !d.focus(t) {
        log::warn!("paste: couldn't bring the dictation window back");
        return copy_instead(d, req, "window-lost");
    }
    if !was_front {
        d.pause(Duration::from_millis(20));
    }
    if d.foreground() != Some(t) {
        return copy_instead(d, req, "window-lost");
    }
    if d.is_own(t) {
        return copy_instead(d, req, "own-window");
    }
    match d.reach(t) {
        Reach::Reachable => {}
        Reach::Blocked => return copy_instead(d, req, "elevated"),
        Reach::Unknown => return copy_instead(d, req, "unverified-window"),
    }
    if !req.permission.allowed() {
        return withheld();
    }

    let lease = match d.begin_paste(&req.text) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("paste: clipboard {e:?}");
            return Outcome::Failed("clipboard");
        }
    };
    d.pause(Duration::from_millis(10));

    // Last look before the keystroke.
    if !req.permission.allowed() {
        restore_quietly(d, &lease);
        return withheld();
    }
    if !d.window_exists(t)
        || d.identity(t) != (target.process_id, target.thread_id)
        || d.foreground() != Some(t)
        || d.reach(t) != Reach::Reachable
    {
        // The user moved on while the clipboard was being set; keep the text for them.
        return Outcome::Copied("window-lost");
    }
    if !d.lease_owned(&lease) {
        // Something else was copied meanwhile: sending Ctrl+V would paste *that*.
        return Outcome::Skipped("clipboard-changed");
    }
    let terminal = d.is_terminal(t);
    match d.send_paste(terminal) {
        Sent::All => {}
        Sent::Partial => return Outcome::Failed("input-partial"), // transcript stays on the clipboard
        Sent::Blocked => return Outcome::Failed("input-blocked"),
    }

    d.pause(RESTORE_DELAY);
    // Put back what was there before (or nothing), unless something new was copied meanwhile.
    restore_quietly(d, &lease);
    Outcome::Attempted
}

fn restore_quietly<D: Desktop>(d: &D, lease: &D::Lease) {
    match d.restore(lease) {
        RestoreOutcome::Restored | RestoreOutcome::Changed => {}
        other => log::warn!("paste: clipboard restore: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const TARGET: Window = Window(100);

    type Hook = (&'static str, Box<dyn Fn(&Fake)>);

    /// A scripted desktop. Every field can be changed between calls through `hook`, which runs
    /// before each query so a test can move the world at an exact step.
    struct Fake {
        exists: Cell<bool>,
        identity: Cell<(u32, u32)>,
        front: Cell<Option<Window>>,
        focus_ok: Cell<bool>,
        own: Cell<bool>,
        reach: Cell<Reach>,
        begin: Cell<Result<(), ClipboardError>>,
        owned: Cell<bool>,
        sent: Cell<Sent>,
        copy: Cell<Result<(), ClipboardError>>,
        log: RefCell<Vec<&'static str>>,
        // Runs just before the named step.
        hook: RefCell<Option<Hook>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                exists: Cell::new(true),
                identity: Cell::new((7, 8)),
                front: Cell::new(Some(TARGET)),
                focus_ok: Cell::new(true),
                own: Cell::new(false),
                reach: Cell::new(Reach::Reachable),
                begin: Cell::new(Ok(())),
                owned: Cell::new(true),
                sent: Cell::new(Sent::All),
                copy: Cell::new(Ok(())),
                log: RefCell::new(Vec::new()),
                hook: RefCell::new(None),
            }
        }

        fn at(&self, step: &'static str) {
            self.log.borrow_mut().push(step);
            let hook = self.hook.borrow_mut().take();
            if let Some((when, f)) = hook {
                if when == step {
                    f(self);
                } else {
                    *self.hook.borrow_mut() = Some((when, f));
                }
            }
        }

        fn did(&self, step: &str) -> bool {
            self.log.borrow().contains(&step)
        }
    }

    impl Desktop for Fake {
        type Lease = ();
        fn window_exists(&self, _: Window) -> bool {
            self.exists.get()
        }
        fn identity(&self, _: Window) -> (u32, u32) {
            self.identity.get()
        }
        fn foreground(&self) -> Option<Window> {
            self.front.get()
        }
        fn focus(&self, w: Window) -> bool {
            self.at("focus");
            if self.focus_ok.get() {
                self.front.set(Some(w));
            }
            self.focus_ok.get()
        }
        fn is_own(&self, _: Window) -> bool {
            self.own.get()
        }
        fn reach(&self, _: Window) -> Reach {
            self.reach.get()
        }
        fn is_terminal(&self, _: Window) -> bool {
            false
        }
        fn begin_paste(&self, _: &str) -> Result<(), ClipboardError> {
            self.at("begin");
            self.begin.get()
        }
        fn lease_owned(&self, _: &()) -> bool {
            self.owned.get()
        }
        fn send_paste(&self, _: bool) -> Sent {
            self.at("send");
            self.sent.get()
        }
        fn restore(&self, _: &()) -> RestoreOutcome {
            self.at("restore");
            RestoreOutcome::Restored
        }
        fn copy_private(&self, _: &str) -> Result<(), ClipboardError> {
            self.at("copy");
            self.copy.get()
        }
        fn pause(&self, d: Duration) {
            // The step after the clipboard is set (the 10 ms settle) is where the world can
            // change under us.
            if d == Duration::from_millis(10) {
                self.at("settle");
            }
        }
    }

    fn request(gate: &Arc<DeliveryGate>, target: Option<PasteTarget>) -> PasteRequest {
        PasteRequest {
            session: SessionId(1),
            row_id: 1,
            text: "hello".into(),
            target,
            incomplete: false,
            permission: gate.permit(),
        }
    }

    fn target() -> Option<PasteTarget> {
        Some(PasteTarget {
            hwnd: TARGET,
            process_id: 7,
            thread_id: 8,
        })
    }

    #[test]
    fn pastes_into_the_proven_target() {
        let f = Fake::new();
        let gate = DeliveryGate::new();
        assert_eq!(deliver(&f, &request(&gate, target())), Outcome::Attempted);
        assert!(f.did("send") && f.did("restore"));
    }

    #[test]
    fn no_recorded_target_copies_and_never_uses_the_current_foreground() {
        let f = Fake::new();
        let gate = DeliveryGate::new();
        assert_eq!(
            deliver(&f, &request(&gate, None)),
            Outcome::Copied("no-window")
        );
        assert!(f.did("copy") && !f.did("send") && !f.did("begin"));
    }

    #[test]
    fn a_recycled_handle_is_a_different_window() {
        let f = Fake::new();
        f.identity.set((99, 8)); // same handle value, another process
        let gate = DeliveryGate::new();
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("window-closed")
        );
        assert!(!f.did("send") && !f.did("focus"));
    }

    #[test]
    fn closed_window_and_failed_focus_copy_instead() {
        let f = Fake::new();
        f.exists.set(false);
        let gate = DeliveryGate::new();
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("window-closed")
        );
        let f = Fake::new();
        f.front.set(Some(Window(5)));
        f.focus_ok.set(false);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("window-lost")
        );
        assert!(!f.did("send"));
    }

    #[test]
    fn own_window_and_integrity_problems_copy() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        f.own.set(true);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("own-window")
        );
        let f = Fake::new();
        f.reach.set(Reach::Blocked);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("elevated")
        );
        let f = Fake::new();
        f.reach.set(Reach::Unknown);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("unverified-window")
        );
        assert!(!f.did("send"));
    }

    #[test]
    fn a_revoked_or_blocked_permission_touches_nothing() {
        let gate = DeliveryGate::new();
        let req = request(&gate, target());
        req.permission.revoke();
        let f = Fake::new();
        assert_eq!(deliver(&f, &req), Outcome::Skipped("withheld"));
        assert!(!f.did("begin") && !f.did("copy") && !f.did("send"));

        let req = request(&gate, target());
        gate.block(); // lock
        assert!(!req.permission.allowed());
        gate.unblock(); // unlock does not revive a permission issued before the lock
        assert!(!req.permission.allowed());
        assert!(request(&gate, target()).permission.allowed());
    }

    #[test]
    fn a_lock_during_the_clipboard_step_restores_and_sends_nothing() {
        let gate = DeliveryGate::new();
        let req = request(&gate, target());
        let g = gate.clone();
        let f = Fake::new();
        *f.hook.borrow_mut() = Some(("settle", Box::new(move |_| g.block())));
        assert_eq!(deliver(&f, &req), Outcome::Skipped("withheld"));
        assert!(f.did("restore") && !f.did("send"));
    }

    #[test]
    fn focus_moving_away_before_the_keys_keeps_the_text_but_sends_nothing() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        *f.hook.borrow_mut() = Some((
            "settle",
            Box::new(|f: &Fake| f.front.set(Some(Window(555)))),
        ));
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Copied("window-lost")
        );
        assert!(!f.did("send") && !f.did("restore"));
    }

    #[test]
    fn someone_elses_copy_is_never_pasted() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        f.owned.set(false);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Skipped("clipboard-changed")
        );
        assert!(!f.did("send"));
    }

    #[test]
    fn partial_and_blocked_input_are_failures_not_attempts() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        f.sent.set(Sent::Partial);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Failed("input-partial")
        );
        let f = Fake::new();
        f.sent.set(Sent::Blocked);
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Failed("input-blocked")
        );
        // The transcript stays on the clipboard for the user in both cases.
        assert!(!f.did("restore"));
    }

    #[test]
    fn clipboard_failure_is_reported_as_such() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        f.begin.set(Err(ClipboardError::Busy));
        assert_eq!(
            deliver(&f, &request(&gate, target())),
            Outcome::Failed("clipboard")
        );
        assert!(!f.did("send"));
    }

    #[test]
    fn copy_fallback_that_fails_is_skipped_not_copied() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        f.copy.set(Err(ClipboardError::Busy));
        assert_eq!(
            deliver(&f, &request(&gate, None)),
            Outcome::Skipped("no-window")
        );
    }
    #[test]
    fn review_lock_during_failed_focus_never_copies() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        let req = request(&gate, target());
        let interrupted = gate.clone();
        *f.hook.borrow_mut() = Some((
            "focus",
            Box::new(move |f: &Fake| {
                interrupted.block();
                f.focus_ok.set(false);
            }),
        ));
        deliver(&f, &req);
        assert!(
            !f.did("copy"),
            "A revoked delivery must not publish clipboard fallback"
        );
    }

    #[test]
    fn review_identity_changed_during_clipboard_step_never_injects() {
        let gate = DeliveryGate::new();
        let f = Fake::new();
        *f.hook.borrow_mut() = Some(("settle", Box::new(|f: &Fake| f.identity.set((99, 88)))));
        deliver(&f, &request(&gate, target()));
        assert!(
            !f.did("send"),
            "The same HWND with changed identity must not receive injection"
        );
    }
}
