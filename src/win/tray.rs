//! Notification-area icon on the ipc window: idle/recording dot, click for history, right-click
//! menu. All calls except `set_recording` run on the ipc thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, PostMessageW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL,
};
use windows::core::w;

/// Shell callback message for the icon.
pub const WM_APP_TRAY: u32 = WM_APP + 3;
/// Posted by `set_recording`; wparam = 1 while recording.
pub const WM_APP_TRAY_STATE: u32 = WM_APP + 4;

const ID: u32 = 1;
const CMD_HISTORY: usize = 1;
const CMD_AUTOSTART: usize = 2;
const CMD_QUIT: usize = 3;
const CMD_SETTINGS: usize = 4;

static RECORDING: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub enum Action {
    None,
    History,
    Settings,
    Autostart(bool),
    Quit,
}

/// A filled circle with a soft edge, as a 32-bit alpha icon.
fn dot(rgb: (u8, u8, u8)) -> HICON {
    const N: i32 = 32;
    let mut px = vec![0u32; (N * N) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let a = (11.5 - d).clamp(0.0, 1.0);
            if a > 0.0 {
                let pm = |v: u8| (v as f32 * a) as u32; // premultiplied
                px[(y * N + x) as usize] =
                    ((a * 255.0) as u32) << 24 | pm(rgb.0) << 16 | pm(rgb.1) << 8 | pm(rgb.2);
            }
        }
    }
    let mask = vec![0u8; (N * N / 8) as usize];
    // SAFETY: bitmaps sized to match the buffers; deleted after the icon copies them.
    unsafe {
        let color = CreateBitmap(N, N, 1, 32, Some(px.as_ptr().cast()));
        let mask = CreateBitmap(N, N, 1, 1, Some(mask.as_ptr().cast()));
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ID,
        ..Default::default()
    }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16().chain([0])) {
        *d = s;
    }
}

fn apply(hwnd: HWND, op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
    let recording = RECORDING.load(Ordering::Relaxed);
    let icon = if recording {
        dot((0xE0, 0x3C, 0x31))
    } else {
        dot((0x3B, 0x82, 0xF6))
    };
    let mut nid = data(hwnd);
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = icon;
    set_tip(
        &mut nid,
        if recording {
            "gemdict — listening"
        } else {
            "gemdict (Ctrl+Win to dictate)"
        },
    );
    // SAFETY: nid is fully initialised; the shell copies the icon, so ours is destroyed.
    unsafe {
        if !Shell_NotifyIconW(op, &nid).as_bool() && op == NIM_ADD {
            log::warn!("tray icon add failed");
        }
        if op == NIM_ADD {
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        let _ = DestroyIcon(icon);
    }
}

pub fn add(hwnd: HWND) {
    // SAFETY: registering a well-known message name.
    TASKBAR_CREATED.store(
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        Ordering::Relaxed,
    );
    apply(hwnd, NIM_ADD);
}

pub fn remove(hwnd: HWND) {
    // SAFETY: deleting our own icon.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
    }
}

/// Explorer restarted: the icon has to be added again.
pub fn is_taskbar_created(msg: u32) -> bool {
    let m = TASKBAR_CREATED.load(Ordering::Relaxed);
    m != 0 && msg == m
}

/// From any thread.
pub fn set_recording(on: bool) {
    RECORDING.store(on, Ordering::Relaxed);
    super::ipc::post_wparam(WM_APP_TRAY_STATE, on as usize);
}

pub fn refresh(hwnd: HWND) {
    apply(hwnd, NIM_MODIFY);
}

/// Handles the icon's callback message.
pub fn on_callback(hwnd: HWND, lparam: LPARAM) -> Action {
    match (lparam.0 & 0xFFFF) as u32 {
        WM_LBUTTONUP => Action::History,
        WM_CONTEXTMENU => menu(hwnd),
        _ => Action::None,
    }
}

fn menu(hwnd: HWND) -> Action {
    let autostart = crate::autostart::enabled();
    // SAFETY: a popup menu owned and destroyed here; SetForegroundWindow + WM_NULL is the
    // documented dance so the menu closes when clicking elsewhere.
    unsafe {
        let Ok(m) = CreatePopupMenu() else {
            return Action::None;
        };
        let _ = AppendMenuW(m, MF_STRING, CMD_HISTORY, w!("History"));
        let _ = AppendMenuW(m, MF_STRING, CMD_SETTINGS, w!("Settings"));
        let check = if autostart { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(
            m,
            MF_STRING | check,
            CMD_AUTOSTART,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        match cmd.0 as usize {
            CMD_HISTORY => Action::History,
            CMD_SETTINGS => Action::Settings,
            CMD_AUTOSTART => Action::Autostart(!autostart),
            CMD_QUIT => Action::Quit,
            _ => Action::None,
        }
    }
}
