//! Paste worker: one job at a time, after the history row is committed.

use crate::event::Event;
use crate::win::overlay::Tone;
use crate::win::window::Window;
use crate::win::{clipboard, input, window};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::sleep;
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

pub enum Job {
    Paste(PasteJob),
    /// Copy from history: just set the clipboard (kept out of clipboard history).
    Copy(String),
}

pub struct PasteJob {
    pub row_id: i64,
    pub text: String,
    /// Foreground window when dictation stopped.
    pub target: Option<Window>,
    /// The text may be cut off (Gemini failed part-way).
    pub incomplete: bool,
}

/// Stored in `transcriptions.paste`.
#[derive(Debug, PartialEq)]
pub enum Outcome {
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
            Outcome::Skipped(_) => ("Saved to history — no text box to paste into", Tone::Error),
            Outcome::Failed("input-blocked") => {
                ("Couldn't paste — text is on the clipboard", Tone::Error)
            }
            Outcome::Failed(_) => ("Couldn't paste — text is in history", Tone::Error),
        })
    }
}

pub struct Pasted {
    pub row_id: i64,
    pub outcome: Outcome,
    pub incomplete: bool,
}

/// Starts the paste thread. Results come back as `Event::Pasted`.
pub fn spawn(events: Sender<Event>) -> Sender<Job> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("paste".into())
        .spawn(move || run(rx, events))
        .expect("spawn paste thread");
    tx
}

fn run(jobs: Receiver<Job>, events: Sender<Event>) {
    let owner = match window::message_window() {
        Ok(h) => h,
        Err(e) => {
            log::error!("paste: no clipboard window: {e}");
            return;
        }
    };
    for job in jobs {
        let job = match job {
            Job::Paste(j) => j,
            Job::Copy(text) => {
                // A deliberate copy: let it into clipboard history like any other.
                if clipboard::set_text(owner, &text, false).is_none() {
                    log::warn!("copy: clipboard busy");
                }
                continue;
            }
        };
        let outcome = paste(&job, owner);
        log::info!("paste row {}: {:?}", job.row_id, outcome);
        let _ = events.send(Event::Pasted(Pasted {
            row_id: job.row_id,
            outcome,
            incomplete: job.incomplete,
        }));
    }
}

/// No text box to paste into: leave the text on the clipboard instead (still private).
fn copy_instead(owner: HWND, text: &str, why: &'static str) -> Outcome {
    match clipboard::set_text(owner, text, true) {
        Some(_) => Outcome::Copied(why),
        None => Outcome::Skipped(why),
    }
}

fn paste(job: &PasteJob, owner: HWND) -> Outcome {
    // Paste only into the window dictation stopped in. If it's gone or won't come back to
    // the front, the user has moved on: don't drop the text into whatever is there now.
    if let Some(t) = job.target {
        if !window::exists(t) {
            return copy_instead(owner, &job.text, "window-closed");
        }
        let was_front = window::foreground() == Some(t);
        if !window::focus(t) {
            log::warn!("paste: couldn't bring the dictation window back");
            return copy_instead(owner, &job.text, "window-lost");
        }
        if !was_front {
            sleep(Duration::from_millis(20));
        }
    }
    let Some(w) = window::foreground() else {
        return copy_instead(owner, &job.text, "no-window");
    };
    if window::is_own(w) {
        return copy_instead(owner, &job.text, "own-window");
    }
    if window::is_elevated(w) {
        return copy_instead(owner, &job.text, "elevated");
    }

    let Some(saved) = clipboard::save(owner) else {
        return Outcome::Failed("clipboard");
    };
    if saved.lossy {
        log::info!("paste: clipboard had formats that won't be restored");
    }
    let Some(seq) = clipboard::set_text(owner, &job.text, true) else {
        return Outcome::Failed("clipboard");
    };

    let class = window::class_name(w);
    let terminal = TERMINAL_CLASSES
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&class))
        || window::exe_name(w).is_some_and(|exe| TERMINAL_EXES.contains(&exe.as_str()));
    sleep(Duration::from_millis(10));
    if !input::paste(terminal) {
        return Outcome::Failed("input-blocked"); // transcript stays on the clipboard
    }

    sleep(RESTORE_DELAY);
    // Put back what was there before (or nothing), unless something new was copied meanwhile.
    if clipboard::sequence() == seq && !clipboard::restore(owner, &saved) {
        log::warn!("paste: clipboard restore failed");
    }
    Outcome::Attempted
}
