#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod core;
mod event;
mod gemini;
mod hotkey;
mod import;
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
use windows::core::w;

/// Roaming: settings and history.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("gemdict")
}

/// Local: log and audio.
fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("gemdict")
}

fn main() {
    #[cfg(debug_assertions)]
    if std::env::args().any(|a| a == "--overlay-demo") {
        overlay_demo();
        return;
    }
    // SAFETY: named mutex kept for the life of the process; the handle is intentionally leaked.
    let _mutex = unsafe { CreateMutexW(None, false, w!("Local\\gemdict-7c1e0d2a")) };
    // SAFETY: plain query right after the create call.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        win::ipc::signal_existing();
        return;
    }
    // SAFETY: process-wide setting made before any window exists.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let dir = data_dir();
    let local = local_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::create_dir_all(&local);
    logger::init(&local.join("gemdict.log"));
    log::info!("gemdict {} starting", env!("CARGO_PKG_VERSION"));
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));

    let store = match store::Store::open(&dir.join("gemdict.db")) {
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
    win::history::init(dir.join("gemdict.db"));
    win::settings_ui::init(dir.join("settings.json"), dir.join("gemdict.db"));
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
    log::info!("gemdict exiting");
    log::logger().flush();
}

/// Debug builds: walks the overlay through its states for eyeballing.
#[cfg(debug_assertions)]
fn overlay_demo() {
    // SAFETY: process-wide setting made before any window exists.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let pause = |s| std::thread::sleep(Duration::from_millis(s));
    overlay::show("Listening…", Tone::Recording, None);
    pause(2500);
    let said = "so the thing I wanted to mention is that the overlay now shows what you are saying while you are still saying it and older lines fade out once there are more than four of them on screen, which keeps the card small and calm even when you ramble on for a good long while about nothing in particular";
    let words: Vec<&str> = said.split(' ').collect();
    for n in 1..=words.len() {
        let finals = words[..n.saturating_sub(3)].join(" ");
        let interim = words[n.saturating_sub(3)..n].join(" ");
        overlay::words(&finals, &interim);
        pause(if n == 12 { 2500 } else { 180 });
    }
    pause(2500);
    overlay::status("Transcribing…", Tone::Busy, None);
    pause(2500);
    overlay::status("Couldn't paste — copied to clipboard", Tone::Error, Some(Duration::from_secs(3)));
    pause(3500);
    overlay::show("Nothing heard", Tone::Info, Some(Duration::from_secs(2)));
    pause(2500);
}
