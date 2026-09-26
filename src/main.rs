#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod event;
mod gemini;
mod hotkey;
mod import;
mod logger;
mod paste;
mod settings;
mod store;
mod win;

use event::Event;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;
use win::overlay::{self, Tone};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::w;

fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("gemdict")
}

fn main() {
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
    let _ = std::fs::create_dir_all(&dir);
    logger::init(&dir.join("gemdict.log"));
    log::info!("gemdict {} starting", env!("CARGO_PKG_VERSION"));

    let settings = settings::Settings::load(&dir.join("settings.json"));
    win::hook::set_chord(settings.chord());

    let (tx, rx) = channel::<Event>();
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || {
                if let Err(e) = win::ipc::run(tx.clone()) {
                    log::error!("ipc thread: {e}");
                }
                let _ = tx.send(Event::Quit);
            })
            .expect("spawn ipc thread");
    }

    // Stub core until capture/live land: proves hotkey → overlay end to end.
    let mut recording = false;
    for ev in rx {
        match ev {
            Event::Toggle => {
                recording = !recording;
                log::info!("toggle -> recording={recording}");
                if recording {
                    overlay::show("Listening…", Tone::Recording, None);
                } else {
                    overlay::show("Stopped", Tone::Info, Some(Duration::from_millis(1200)));
                }
            }
            Event::ShowHistory => overlay::show("History (not built yet)", Tone::Busy, Some(Duration::from_secs(2))),
            Event::Power(p) => log::info!("power: {p:?}"),
            Event::Quit => break,
            _ => {}
        }
    }
    log::info!("gemdict exiting");
    log::logger().flush();
}
