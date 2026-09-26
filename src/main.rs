#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod core;
mod event;
mod gemini;
mod hotkey;
mod import;
mod install;
mod key;
mod logger;
mod paste;
mod settings;
mod sound;
mod store;
mod win;

use event::Event;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;
use win::overlay::{self, Tone};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::{PCWSTR, w};

/// The app's name: data folders, install folder, credential and Run entry.
pub const NAME: &str = env!("CARGO_PKG_NAME");
/// Held for the life of the process: one instance per session.
pub const INSTANCE_MUTEX: PCWSTR = w!("Local\\dictap-7c1e0d2a");

/// Roaming: settings and history.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join(NAME)
}

/// Local: log and audio.
fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join(NAME)
}

fn main() {
    #[cfg(debug_assertions)]
    if std::env::args().any(|a| a == "--overlay-demo") {
        overlay_demo();
        return;
    }
    if install::handle_args() {
        return;
    }
    // SAFETY: process-wide setting made before any window exists.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // Before taking the mutex: installing starts the installed copy, which needs it.
    if install::offer() {
        return;
    }
    // SAFETY: named mutex kept for the life of the process; the handle is intentionally leaked.
    let _mutex = unsafe { CreateMutexW(None, false, INSTANCE_MUTEX) };
    // SAFETY: plain query right after the create call.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        win::ipc::signal_existing(win::app::Page::from_args());
        return;
    }

    let dir = data_dir();
    let local = local_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::create_dir_all(&local);
    logger::init(&local.join(format!("{NAME}.log")));
    log::info!("{NAME} {} starting", env!("CARGO_PKG_VERSION"));
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));

    let db = dir.join(format!("{NAME}.db"));
    let store = match store::Store::open(&db) {
        Ok(s) => s,
        Err(e) => {
            log::error!("history database: {e}");
            overlay::show(
                "Couldn't open history database",
                Tone::Error,
                Some(Duration::from_secs(5)),
            );
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
    };

    let (tx, rx) = channel::<Event>();
    let paths = core::Paths {
        settings: dir.join("settings.json"),
        spool: local.join("spool"),
        failed: local.join("failed"),
    };
    win::app::init(dir.join("settings.json"), db);
    // Core first, so the hook uses the saved chord from the start.
    let core = core::Core::new(paths, store, tx.clone());
    let ipc = {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || {
                if let Err(e) = win::ipc::run(tx.clone()) {
                    log::error!("ipc thread: {e}");
                }
                let _ = tx.send(Event::Quit);
            })
            .expect("spawn ipc thread")
    };
    drop(tx);
    core.run(rx);
    win::ipc::quit();
    let _ = ipc.join();
    log::info!("{NAME} exiting");
    log::logger().flush();
}

/// Debug builds: walks the overlay through its states for eyeballing.
#[cfg(debug_assertions)]
fn overlay_demo() {
    // SAFETY: process-wide setting made before any window exists.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let pause = |ms| std::thread::sleep(Duration::from_millis(ms));
    // A fake voice: syllable-ish bursts.
    let speak = |ms: u64| {
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_millis() < u128::from(ms) {
            let t = t0.elapsed().as_secs_f32();
            let env = ((t * 7.0).sin() * (t * 2.3).sin()).abs();
            overlay::level(0.01 + 0.25 * env);
            pause(20);
        }
    };
    overlay::show("Starting…", Tone::Busy, None);
    pause(900);
    overlay::show("Listening…", Tone::Recording, None);
    // Recording limit: the countdown pill, amber then red.
    overlay::limit(Duration::from_secs(8));
    speak(8200);
    overlay::show("Listening…", Tone::Recording, None);
    speak(1500);
    let said = "Hey Sam, quick update on the launch. The build is green, I pushed the fix for the login bug this morning, and the release notes are drafted, so we should be good to ship on Friday once QA signs off. Let me know if you want to walk through it before then.";
    let words: Vec<&str> = said.split(' ').collect();
    for n in 1..=words.len() {
        let finals = words[..n.saturating_sub(3)].join(" ");
        let interim = words[n.saturating_sub(3)..n].join(" ");
        overlay::words(&finals, &interim);
        speak(if n == 12 { 1500 } else { 170 });
    }
    overlay::words(said, "");
    overlay::level(0.0);
    pause(1200);
    overlay::status("Transcribing…", Tone::Busy, None);
    pause(1500);
    overlay::done();
    pause(1600);
    overlay::show("Listening…", Tone::Recording, None);
    overlay::words("hello there", "");
    pause(800);
    overlay::status(
        "Couldn't paste — copied to clipboard",
        Tone::Error,
        Some(Duration::from_secs(2)),
    );
    pause(2800);
    overlay::show("Nothing heard", Tone::Info, Some(Duration::from_secs(2)));
    pause(2800);
}
