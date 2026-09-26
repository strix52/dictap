//! Small helpers shared by the history and settings windows (ipc thread only).

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetWindowTextLengthW, GetWindowTextW, HMENU, MoveWindow, NONCLIENTMETRICSW,
    SPI_GETNONCLIENTMETRICS, SendMessageW, SetWindowTextW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_SETFONT, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR};

pub fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: a message to a window on this thread; pointer-carrying callers pass valid data.
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))) }.0
}

pub fn dpi(h: HWND) -> u32 {
    // SAFETY: plain query.
    unsafe { GetDpiForWindow(h) }.max(96)
}

/// Scales 96-dpi pixels.
pub fn px(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

/// The system message font at this dpi. The caller owns it.
pub fn font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: ncm is sized; the font is created from its copy.
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(std::ptr::from_mut(&mut ncm).cast()),
            0,
            dpi,
        );
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

/// Creates a visible, tab-stop child control.
pub fn child(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    ex: u32,
    id: usize,
    font: HFONT,
) -> HWND {
    // SAFETY: standard child creation; the id rides in the HMENU slot.
    let h = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(ex),
            class,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style),
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(id as *mut _)),
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    }
    .unwrap_or_default();
    send(h, WM_SETFONT, font.0 as usize, 1);
    h
}

pub fn place(h: HWND, r: RECT) {
    // SAFETY: moving our own child.
    let _ = unsafe { MoveWindow(h, r.left, r.top, r.right - r.left, r.bottom - r.top, true) };
}

pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn get_text(h: HWND) -> String {
    // SAFETY: buffer sized from the length query.
    unsafe {
        let n = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(h, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

pub fn set_text(h: HWND, s: &str) {
    // SAFETY: HSTRING outlives the call.
    let _ = unsafe { SetWindowTextW(h, &HSTRING::from(s)) };
}

/// Unix ms → "2026-09-26 22:36" in local time.
pub fn local_time(ms: i64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ticks = (ms.max(0) as u64) * 10_000 + 116_444_736_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: out-pointers are valid.
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}
