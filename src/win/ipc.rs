//! The IPC thread: a hidden top-level window (so it receives power and session broadcasts,
//! which message-only windows don't), the tray icon and the app window, pumping messages
//! until quit. The keyboard hook has its own thread (see `hook`).
//! A second instance finds the window by class name and asks it to show history.

use crate::event::{Action, ActionRequest, Event, PowerEvent, RequestId};
use std::cell::OnceCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ENDSESSION,
    WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows::core::w;

pub const WM_APP_TOGGLE: u32 = WM_APP + 1;
pub const WM_APP_SHOW: u32 = WM_APP + 2;

const CLASS: windows::core::PCWSTR = w!("dictap.ipc");

/// The IPC window, as an integer so other threads can post to it.
static IPC: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static TX: OnceCell<Sender<Event>> = const { OnceCell::new() };
}

/// Hands an event to the core. False if the core has gone away.
pub(super) fn try_send(ev: Event) -> bool {
    TX.with(|tx| tx.get().is_some_and(|tx| tx.send(ev).is_ok()))
}

fn send(ev: Event) {
    let _ = try_send(ev);
}

fn post(msg: u32) -> bool {
    post_wparam(msg, 0)
}

pub fn post_wparam(msg: u32, wparam: usize) -> bool {
    let hwnd = IPC.load(Ordering::Acquire);
    // SAFETY: posting to a window handle we created; a stale handle just fails.
    hwnd != 0
        && unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(wparam), LPARAM(0)) }
            .is_ok()
}

/// Ends the hook thread (from any thread).
pub fn quit() {
    post(WM_CLOSE);
}

/// If another dictap is running, asks it to show `page` and returns true.
pub fn signal_existing(page: super::app::Page) -> bool {
    // SAFETY: plain lookup by class name.
    match unsafe { FindWindowW(CLASS, None) } {
        Ok(hwnd) if !hwnd.is_invalid() => {
            // SAFETY: posting a private message to the other instance's window.
            unsafe { PostMessageW(Some(hwnd), WM_APP_SHOW, WPARAM(page as usize), LPARAM(0)) }
                .is_ok()
        }
        _ => false,
    }
}

/// If another instance is running, asks it to quit and returns true.
pub fn close_existing() -> bool {
    // SAFETY: plain lookup by class name, then a post to that window.
    match unsafe { FindWindowW(CLASS, None) } {
        Ok(hwnd) if !hwnd.is_invalid() => {
            unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok()
        }
        _ => false,
    }
}

/// Runs the hook thread's message loop until `quit()`.
pub fn run(tx: Sender<Event>) -> windows::core::Result<()> {
    TX.with(|t| t.set(tx).ok());
    // SAFETY: standard window class registration and creation; `wndproc` matches WNDPROC.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            CLASS,
            w!("dictap"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    IPC.store(hwnd.0 as isize, Ordering::Release);
    // SAFETY: registering our own window for session notifications.
    if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        log::warn!("session notifications unavailable: {e}");
    }
    super::hook::start(hwnd)?;
    super::tray::add(hwnd);

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window and hook.
    loop {
        // SAFETY: as above.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        match super::classify_get_message(got) {
            super::Pump::Error => {
                log::error!("ipc message loop: GetMessageW failed");
                break;
            }
            super::Pump::Quit => break,
            super::Pump::Message => {}
        }
        if super::app::pre_translate(&msg) {
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    super::hook::stop();
    IPC.store(0, Ordering::Release);
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_TOGGLE => send(Event::Toggle),
        WM_APP_SHOW => super::app::show(super::app::Page::from_index(wparam.0)),
        WM_POWERBROADCAST => match wparam.0 as u32 {
            PBT_APMSUSPEND => send(Event::Power(PowerEvent::Suspend)),
            PBT_APMRESUMEAUTOMATIC => {
                super::hook::reinstall();
                send(Event::Power(PowerEvent::Resume));
            }
            _ => {}
        },
        WM_WTSSESSION_CHANGE => match wparam.0 as u32 {
            WTS_SESSION_LOCK => send(Event::Power(PowerEvent::Lock)),
            WTS_SESSION_UNLOCK => {
                super::hook::reinstall();
                send(Event::Power(PowerEvent::Unlock));
            }
            _ => {}
        },
        WM_ENDSESSION if wparam.0 != 0 => send(Event::Quit),
        WM_CLOSE => {
            // SAFETY: destroying our own window on its thread.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => {
            super::tray::remove(hwnd);
            unsafe { PostQuitMessage(0) }
        }
        super::tray::WM_APP_TRAY_STATE => super::tray::refresh(hwnd),
        super::tray::WM_APP_TRAY => match super::tray::on_callback(hwnd, lparam) {
            super::tray::Action::History => super::app::show(super::app::Page::History),
            super::tray::Action::Settings => super::app::show(super::app::Page::Settings),
            super::tray::Action::CopyLatest => send(Event::Ui(ActionRequest {
                id: RequestId::next(),
                window_generation: 0,
                action: Action::CopyLatest,
            })),
            // The tray has no window to answer; generation 0 never matches one.
            super::tray::Action::Autostart(on) => {
                send(Event::Ui(ActionRequest {
                    id: RequestId::next(),
                    window_generation: 0,
                    action: Action::SetAutostart(on),
                }));
            }
            super::tray::Action::Quit => send(Event::Quit),
            super::tray::Action::None => {}
        },
        m if super::tray::is_taskbar_created(m) => super::tray::add(hwnd),
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
