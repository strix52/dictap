//! Paste worker: one job at a time, after the history row is committed.

use crate::event::Event;
use crate::win::{clipboard, input, window};
use crate::win::window::Window;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::sleep;
use std::time::Duration;

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

pub struct Job {
    pub row_id: i64,
    pub text: String,
    /// Foreground window when dictation stopped.
    pub target: Option<Window>,
}

/// Stored in `transcriptions.paste`.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Attempted,
    Skipped(&'static str),
    Failed(&'static str),
}

impl Outcome {
    pub fn db_value(&self) -> String {
        match self {
            Outcome::Attempted => "attempted".into(),
            Outcome::Skipped(r) => format!("skipped:{r}"),
            Outcome::Failed(r) => format!("failed:{r}"),
        }
    }

    /// Notice for the overlay when the text didn't go in.
    pub fn notice(&self) -> Option<&'static str> {
        match self {
            Outcome::Attempted => None,
            Outcome::Skipped("elevated") => Some("Saved to history — can't paste into an admin window"),
            Outcome::Skipped(_) => Some("Saved to history — no text box to paste into"),
            Outcome::Failed("input-blocked") => Some("Couldn't paste — text is on the clipboard"),
            Outcome::Failed(_) => Some("Couldn't paste — text is in history"),
        }
    }
}

pub struct Pasted {
    pub row_id: i64,
    pub outcome: Outcome,
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
        let outcome = paste(&job, owner);
        log::info!("paste row {}: {:?}", job.row_id, outcome);
        let _ = events.send(Event::Pasted(Pasted { row_id: job.row_id, outcome }));
    }
}

fn paste(job: &Job, owner: windows::Win32::Foundation::HWND) -> Outcome {
    // Prefer the window dictation started from; fall back to whatever is focused now.
    let restored = job.target.filter(|&t| window::exists(t) && !window::is_own(t)).is_some_and(window::focus);
    let Some(w) = window::foreground() else {
        return Outcome::Skipped("no-window");
    };
    if window::is_own(w) {
        return Outcome::Skipped("own-window");
    }
    if window::is_elevated(w) {
        return Outcome::Skipped("elevated");
    }
    if job.target.is_some() && !restored {
        log::warn!("paste: couldn't refocus the dictation window; pasting into the current one");
    }
    if restored {
        sleep(Duration::from_millis(20));
    }

    let Some(saved) = clipboard::save(owner) else {
        return Outcome::Failed("clipboard");
    };
    let Some(seq) = clipboard::set_text(owner, &job.text) else {
        return Outcome::Failed("clipboard");
    };

    let class = window::class_name(w);
    let terminal = TERMINAL_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(&class))
        || window::exe_name(w).is_some_and(|exe| TERMINAL_EXES.contains(&exe.as_str()));
    sleep(Duration::from_millis(10));
    if !input::paste(terminal) {
        return Outcome::Failed("input-blocked"); // transcript stays on the clipboard
    }

    sleep(RESTORE_DELAY);
    // Only restore if nobody copied something new meanwhile, and there's something to restore.
    if clipboard::sequence() == seq && !saved.is_empty() && !clipboard::restore(owner, &saved) {
        log::warn!("paste: clipboard restore failed");
    }
    Outcome::Attempted
}
