//! Status pill near the bottom of the screen: layered, topmost, click-through, never
//! takes focus. Runs on its own thread, started on first use.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CreateFontW, CreateSolidBrush, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, Ellipse, EndPaint, FW_SEMIBOLD, FillRect, GetMonitorInfoW, GetTextExtentPoint32W, HDC,
    HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, PAINTSTRUCT, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT, HGDIOBJ, CreateRoundRectRgn, SetWindowRgn, GetDC, ReleaseDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, HWND_TOPMOST, KillTimer, LWA_ALPHA,
    MSG, PostMessageW, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
    SetTimer, SetWindowPos, ShowWindow, WM_APP, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Recording,
    Busy,
    Info,
    Error,
}

impl Tone {
    fn dot(self) -> COLORREF {
        match self {
            Tone::Recording => rgb(230, 57, 70),
            Tone::Busy => rgb(160, 160, 170),
            Tone::Info => rgb(46, 196, 120),
            Tone::Error => rgb(245, 166, 35),
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

const WM_APP_UPDATE: u32 = WM_APP + 10;
const HIDE_TIMER: usize = 1;

static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
static STATE: Mutex<(String, Tone)> = Mutex::new((String::new(), Tone::Info));
static STARTED: OnceLock<()> = OnceLock::new();

/// Shows `text`; hides after `hide_after` if given, else stays until the next call.
pub fn show(text: &str, tone: Tone, hide_after: Option<Duration>) {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = (text.to_string(), tone);
    post(hide_after.map_or(0, |d| d.as_millis().max(1) as usize));
}

pub fn hide() {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).0.clear();
    post(0);
}

fn post(hide_ms: usize) {
    STARTED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || run(tx))
            .expect("spawn overlay thread");
        let _ = rx.recv();
    });
    let hwnd = HWND_VAL.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: posting to our overlay window.
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_UPDATE, WPARAM(hide_ms), LPARAM(0)) };
    }
}

fn run(ready: std::sync::mpsc::Sender<()>) {
    // SAFETY: standard class registration, window creation and message loop on this thread.
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            let _ = ready.send(());
            return;
        };
        let class = w!("gemdict.overlay");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinstance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        match CreateWindowExW(ex, class, w!("gemdict"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinstance.into()), None) {
            Ok(hwnd) => {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
                HWND_VAL.store(hwnd.0 as isize, Ordering::Release);
            }
            Err(e) => log::error!("overlay window: {e}"),
        }
        let _ = ready.send(());
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn font(dpi: u32) -> HFONT {
    let px = -(15 * dpi as i32 / 96);
    // SAFETY: creates a GDI font the caller deletes.
    unsafe {
        CreateFontW(px, 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0, Default::default(), Default::default(),
            Default::default(), CLEARTYPE_QUALITY, 0, w!("Segoe UI"))
    }
}

/// Layout in physical pixels for the current text: (width, height, padding, dot size).
fn layout(hdc: HDC, text: &[u16], dpi: u32) -> (i32, i32, i32, i32) {
    let s = |v: i32| v * dpi as i32 / 96;
    let mut size = SIZE::default();
    // SAFETY: valid DC and buffers.
    let _ = unsafe { GetTextExtentPoint32W(hdc, text, &mut size) };
    let (pad, dot) = (s(14), s(10));
    (pad + dot + s(8) + size.cx + pad, s(36), pad, dot)
}

fn update(hwnd: HWND, hide_ms: usize) {
    let (text, _) = STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // SAFETY: all calls operate on our own window on its thread.
    unsafe {
        let _ = KillTimer(Some(hwnd), HIDE_TIMER);
        if text.is_empty() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let dpi = GetDpiForWindow(hwnd).max(96);

        let wide: Vec<u16> = text.encode_utf16().collect();
        let hdc = GetDC(Some(hwnd));
        let f = font(dpi);
        let old = SelectObject(hdc, HGDIOBJ(f.0));
        let (w, h, _, _) = layout(hdc, &wide, dpi);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(f.0));
        ReleaseDC(Some(hwnd), hdc);

        let work = info.rcWork;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.bottom - h - 48 * dpi as i32 / 96;
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, h, h);
        SetWindowRgn(hwnd, Some(rgn), false);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = InvalidateRect(Some(hwnd), None, true);
        if hide_ms > 0 {
            SetTimer(Some(hwnd), HIDE_TIMER, hide_ms as u32, None);
        }
    }
}

fn paint(hwnd: HWND) {
    let (text, tone) = STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: paint cycle on our own window; every GDI object created here is deleted.
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let f = font(dpi);
        let old = SelectObject(hdc, HGDIOBJ(f.0));
        let (w, h, pad, dot) = layout(hdc, &wide, dpi);

        let bg = CreateSolidBrush(rgb(32, 33, 36));
        FillRect(hdc, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
        let dot_brush = CreateSolidBrush(tone.dot());
        let old_brush = SelectObject(hdc, HGDIOBJ(dot_brush.0));
        let top = (h - dot) / 2;
        let _ = Ellipse(hdc, pad, top, pad + dot, top + dot);
        SelectObject(hdc, old_brush);

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, rgb(240, 240, 240));
        let mut rect = RECT { left: pad + dot + 8 * dpi as i32 / 96, top: 0, right: w, bottom: h };
        let mut buf = wide.clone();
        DrawTextW(hdc, &mut buf, &mut rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(f.0));
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let _ = DeleteObject(HGDIOBJ(dot_brush.0));
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_UPDATE => update(hwnd, wparam.0),
        WM_PAINT => paint(hwnd),
        WM_TIMER if wparam.0 == HIDE_TIMER => {
            STATE.lock().unwrap_or_else(|e| e.into_inner()).0.clear();
            update(hwnd, 0);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
