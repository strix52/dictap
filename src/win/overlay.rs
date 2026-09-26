//! The floating HUD near the bottom of the screen: a status pill that grows into a live
//! transcript card while Gemini streams. Layered with per-pixel alpha, topmost,
//! click-through, never takes focus. Runs on its own thread, started on first use; it
//! repaints only when told to (no timers except a one-shot hide).
//!
//! Rendering: GDI draws the text as a grey coverage mask into a DIB (grey level =
//! opacity), then the card, border, shadow, dot and text are composited per pixel into
//! premultiplied BGRA and handed to `UpdateLayeredWindow`.

use super::ui;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, CreateFontW, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, FW_NORMAL, FW_SEMIBOLD, GdiFlush, GetDC, GetMonitorInfoW,
    GetTextExtentPoint32W, GetTextMetricsW, HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMessageW, HWND_TOPMOST, KillTimer, MA_NOACTIVATE, MSG, PostMessageW, RegisterClassW,
    SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetTimer, SetWindowPos,
    ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_APP, WM_MOUSEACTIVATE, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::{HSTRING, w};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Recording,
    Busy,
    Info,
    Error,
}

impl Tone {
    fn dot(self) -> Rgb {
        match self {
            Tone::Recording => Rgb(0xE2, 0x3B, 0x32),
            Tone::Busy => Rgb(0xA0, 0xA0, 0xA8),
            Tone::Info => Rgb(0x8A, 0xB4, 0xF8),
            Tone::Error => Rgb(0xF5, 0xA6, 0x23),
        }
    }
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

// The HUD is always dark: it floats over whatever app is underneath.
const CARD: Rgb = Rgb(0x1A, 0x1A, 0x1C);
const CARD_ALPHA: f32 = 0.94;
const BORDER_ALPHA: f32 = 0.10;
const INK: Rgb = Rgb(0xF5, 0xF5, 0xF6);
const SHADOW_ALPHA: f32 = 0.28;

// Layout in 96-dpi px.
const MARGIN: i32 = 12; // room for the shadow around the card
const BOTTOM_GAP: i32 = 24;
const PILL_H: i32 = 36;
const PAD_X: i32 = 14;
const PAD_Y: i32 = 12;
const DOT: i32 = 8;
const DOT_GAP: i32 = 8;
const HEADER_H: i32 = 20;
const HEADER_GAP: i32 = 8;
const LINE_H: i32 = 22;
const MAX_LINES: usize = 4;
const CARD_MIN_W: i32 = 320;
const CARD_MAX_W: i32 = 520;
const PILL_RADIUS: i32 = 18;
const CARD_RADIUS: i32 = 16;
const LABEL_PX: i32 = 13;
const TEXT_PX: i32 = 15;
/// Interim words, not yet final.
const INTERIM: f32 = 0.6;
/// When the transcript runs past MAX_LINES, the oldest visible lines dim.
const FADE: [f32; MAX_LINES] = [0.45, 0.70, 1.0, 1.0];

const WM_APP_UPDATE: u32 = WM_APP + 10;
const HIDE_TIMER: usize = 1;

#[derive(Default)]
struct View {
    label: String,
    tone: Option<Tone>,
    finals: String,
    interim: String,
    /// The card's width only grows while it's up, so text doesn't jump sideways.
    locked_w: i32,
}

static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
static VIEW: Mutex<View> = Mutex::new(View {
    label: String::new(),
    tone: None,
    finals: String::new(),
    interim: String::new(),
    locked_w: 0,
});
static STARTED: OnceLock<()> = OnceLock::new();

fn view() -> std::sync::MutexGuard<'static, View> {
    VIEW.lock().unwrap_or_else(|e| e.into_inner())
}

/// A status message as a pill; clears any transcript. Hides after `hide_after` if given.
pub fn show(text: &str, tone: Tone, hide_after: Option<Duration>) {
    {
        let mut v = view();
        *v = View {
            label: text.to_string(),
            tone: Some(tone),
            ..View::default()
        };
    }
    post(hide_after);
}

/// Changes the header but keeps the transcript on screen (e.g. "Transcribing…").
pub fn status(text: &str, tone: Tone, hide_after: Option<Duration>) {
    {
        let mut v = view();
        v.label = text.to_string();
        v.tone = Some(tone);
    }
    post(hide_after);
}

/// The live transcript so far: settled text and the interim tail.
pub fn words(finals: &str, interim: &str) {
    {
        let mut v = view();
        if v.tone.is_none() {
            return; // hidden meanwhile
        }
        v.finals = finals.to_string();
        v.interim = interim.to_string();
    }
    post(None);
}

pub fn hide() {
    *view() = View::default();
    post(None);
}

fn post(hide_after: Option<Duration>) {
    STARTED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || run(tx))
            .expect("spawn overlay thread");
        let _ = rx.recv();
    });
    let hide_ms = hide_after.map_or(0, |d| d.as_millis().max(1) as usize);
    let hwnd = HWND_VAL.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: posting to our overlay window.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_APP_UPDATE,
                WPARAM(hide_ms),
                LPARAM(0),
            )
        };
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
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let ex =
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        match CreateWindowExW(
            ex,
            class,
            w!("gemdict"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(hwnd) => HWND_VAL.store(hwnd.0 as isize, Ordering::Release),
            Err(e) => log::error!("overlay window: {e}"),
        }
        let _ = ready.send(());
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// Work area and dpi of the monitor the user is looking at: the foreground window's,
/// else the cursor's.
fn target_monitor() -> (windows::Win32::Foundation::RECT, u32) {
    // SAFETY: plain queries with valid out-pointers.
    unsafe {
        let fg = GetForegroundWindow();
        let monitor = if fg.0.is_null() {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST)
        } else {
            MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let (mut dx, mut dy) = (96, 96);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (info.rcWork, dx.max(96))
    }
}

fn font(px: i32, weight: i32) -> HFONT {
    // GDI exposes the variable font's named instances as separate families.
    let face = match (ui::face(), weight >= FW_SEMIBOLD.0 as i32) {
        ("Segoe UI Variable Text", true) => "Segoe UI Variable Text Semibold",
        (f, _) => f,
    };
    // SAFETY: creates a GDI font the caller deletes. Greyscale AA: the glyphs become an
    // alpha mask, which ClearType's coloured fringes would spoil.
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            Default::default(),
            Default::default(),
            Default::default(),
            ANTIALIASED_QUALITY,
            0,
            &HSTRING::from(face),
        )
    }
}

fn text_w(hdc: HDC, s: &[u16]) -> i32 {
    let mut size = SIZE::default();
    // SAFETY: valid DC and buffer.
    let _ = unsafe { GetTextExtentPoint32W(hdc, s, &mut size) };
    size.cx
}

fn line_height(hdc: HDC) -> i32 {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: valid DC.
    let _ = unsafe { GetTextMetricsW(hdc, &mut tm) };
    tm.tmHeight
}

/// A word placed on a line, with its opacity.
struct Word {
    text: Vec<u16>,
    x: i32,
    alpha: f32,
}

/// Greedy word wrap of finals (opaque) then interim (dimmed). Returns every line.
fn wrap(hdc: HDC, finals: &str, interim: &str, width: i32) -> Vec<Vec<Word>> {
    let space = text_w(hdc, &[b' ' as u16]);
    let mut lines: Vec<Vec<Word>> = vec![Vec::new()];
    let mut x = 0;
    let tokens = finals
        .split_whitespace()
        .map(|t| (t, 1.0))
        .chain(interim.split_whitespace().map(|t| (t, INTERIM)));
    for (t, alpha) in tokens {
        let text: Vec<u16> = t.encode_utf16().collect();
        let tw = text_w(hdc, &text);
        let line = lines.last_mut().expect("never empty");
        if !line.is_empty() && x + space + tw > width {
            lines.push(Vec::new());
            x = 0;
        } else if !line.is_empty() {
            x += space;
        }
        lines
            .last_mut()
            .expect("never empty")
            .push(Word { text, x, alpha });
        x += tw;
    }
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// Everything needed to paint one frame, in physical pixels relative to the window.
struct Frame {
    win_w: i32,
    win_h: i32,
    card: (i32, i32, i32, i32), // x, y, w, h
    radius: i32,
    dot: (i32, i32, i32), // centre x, centre y, diameter
    tone: Tone,
}

fn update(hwnd: HWND, hide_ms: usize) {
    // SAFETY: our own window on its thread.
    unsafe {
        let _ = KillTimer(Some(hwnd), HIDE_TIMER);
    }
    let (label, tone, finals, interim, locked) = {
        let v = view();
        (
            v.label.clone(),
            v.tone,
            v.finals.clone(),
            v.interim.clone(),
            v.locked_w,
        )
    };
    let Some(tone) = tone else {
        // SAFETY: our own window.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        return;
    };
    let (work, dpi) = target_monitor();
    let s = |v: i32| v * dpi as i32 / 96;
    let locked = render(hwnd, &label, tone, &finals, &interim, locked, work, s);
    view().locked_w = locked;
    if hide_ms > 0 {
        // SAFETY: timer on our own window.
        unsafe { SetTimer(Some(hwnd), HIDE_TIMER, hide_ms as u32, None) };
    }
}

/// Lays out, rasterises and shows one frame. Returns the card width to lock.
#[allow(clippy::too_many_arguments)]
fn render(
    hwnd: HWND,
    label: &str,
    tone: Tone,
    finals: &str,
    interim: &str,
    locked: i32,
    work: windows::Win32::Foundation::RECT,
    s: impl Fn(i32) -> i32,
) -> i32 {
    // SAFETY: every GDI object created here is selected out and deleted before returning.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let label_font = font(s(LABEL_PX), FW_SEMIBOLD.0 as i32);
        let text_font = font(s(TEXT_PX), FW_NORMAL.0 as i32);
        let old_font = SelectObject(dc, HGDIOBJ(label_font.0));
        let label_w16: Vec<u16> = label.encode_utf16().collect();
        let label_w = text_w(dc, &label_w16);
        let label_h = line_height(dc);

        let max_w = (work.right - work.left - s(32)).max(s(200));
        let has_words = !finals.trim().is_empty() || !interim.trim().is_empty();
        let (mut lines, card_w, card_h, radius, text_h);
        if has_words {
            SelectObject(dc, HGDIOBJ(text_font.0));
            text_h = line_height(dc);
            let natural = text_w(dc, &format!("{finals} {interim}").encode_utf16().collect::<Vec<_>>());
            card_w = (natural + 2 * s(PAD_X))
                .clamp(s(CARD_MIN_W), s(CARD_MAX_W))
                .max(locked)
                .min(max_w);
            lines = wrap(dc, finals, interim, card_w - 2 * s(PAD_X));
            let shown = lines.len().min(MAX_LINES) as i32;
            card_h = s(PAD_Y) + s(HEADER_H) + s(HEADER_GAP) + shown * s(LINE_H) + s(PAD_Y);
            radius = s(CARD_RADIUS);
        } else {
            lines = Vec::new();
            text_h = 0;
            card_w = (s(PAD_X) + s(DOT) + s(DOT_GAP) + label_w + s(PAD_X)).min(max_w);
            card_h = s(PILL_H);
            radius = s(PILL_RADIUS);
        }
        let m = s(MARGIN);
        let (win_w, win_h) = (card_w + 2 * m, card_h + 2 * m);

        // Text mask: grey level = coverage × opacity.
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: win_w,
                biHeight: -win_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(label_font.0));
            let _ = DeleteObject(HGDIOBJ(text_font.0));
            let _ = DeleteDC(dc);
            return locked;
        };
        let old_bmp = SelectObject(dc, HGDIOBJ(bmp.0));
        SetBkMode(dc, TRANSPARENT);
        let grey = |a: f32| {
            let g = (a.clamp(0.0, 1.0) * 255.0).round() as u32;
            COLORREF(g | g << 8 | g << 16)
        };

        // Header / pill label.
        let (header_top, header_h) = if has_words {
            (m + s(PAD_Y), s(HEADER_H))
        } else {
            (m, card_h)
        };
        let label_x = m + s(PAD_X) + s(DOT) + s(DOT_GAP);
        SelectObject(dc, HGDIOBJ(label_font.0));
        SetTextColor(dc, grey(if has_words { 0.72 } else { 1.0 }));
        let _ = TextOutW(dc, label_x, header_top + (header_h - label_h) / 2, &label_w16);

        // Transcript: the last MAX_LINES lines, older ones dimmed when it overflows.
        if has_words {
            SelectObject(dc, HGDIOBJ(text_font.0));
            let overflow = lines.len() > MAX_LINES;
            let skip = lines.len().saturating_sub(MAX_LINES);
            let top = m + s(PAD_Y) + s(HEADER_H) + s(HEADER_GAP);
            for (i, line) in lines.drain(..).skip(skip).enumerate() {
                let fade = if overflow { FADE[i] } else { 1.0 };
                let y = top + i as i32 * s(LINE_H) + (s(LINE_H) - text_h) / 2;
                for word in line {
                    SetTextColor(dc, grey(fade * word.alpha));
                    let _ = TextOutW(dc, m + s(PAD_X) + word.x, y, &word.text);
                }
            }
        }
        let _ = GdiFlush();

        let frame = Frame {
            win_w,
            win_h,
            card: (m, m, card_w, card_h),
            radius,
            dot: (
                m + s(PAD_X) + s(DOT) / 2,
                header_top + header_h / 2,
                s(DOT),
            ),
            tone,
        };
        // SAFETY: the DIB is win_w × win_h 32-bit pixels, owned by bmp until deleted below.
        let px = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (win_w * win_h) as usize);
        composite(px, &frame, s(4));

        // Place it: centred, bottom-anchored so the card grows upwards.
        let x = work.left + (work.right - work.left - win_w) / 2;
        let y = work.bottom - s(BOTTOM_GAP) - card_h - m;
        let screen = GetDC(None);
        let dst = POINT { x, y };
        let size = SIZE {
            cx: win_w,
            cy: win_h,
        };
        let src = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        if let Err(e) = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            Some(&dst),
            Some(&size),
            Some(dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            log::warn!("overlay update: {e}");
        }
        ReleaseDC(None, screen);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        SelectObject(dc, old_bmp);
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteObject(HGDIOBJ(label_font.0));
        let _ = DeleteObject(HGDIOBJ(text_font.0));
        let _ = DeleteDC(dc);
        if has_words { card_w } else { 0 }
    }
}

/// Signed distance from (px, py) to a rounded rectangle; negative inside.
fn rounded_rect_sd(px: f32, py: f32, (x, y, w, h): (f32, f32, f32, f32), r: f32) -> f32 {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let qx = (px - cx).abs() - (w / 2.0 - r);
    let qy = (py - cy).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Premultiplied "over": (colour, alpha) onto a premultiplied BGRA pixel.
fn over(dst: u32, c: Rgb, a: f32) -> u32 {
    if a <= 0.0 {
        return dst;
    }
    let a = a.min(1.0);
    let inv = 1.0 - a;
    let ch = |shift: u32, src: u8| {
        let d = ((dst >> shift) & 0xFF) as f32;
        ((src as f32 * a + d * inv).round() as u32).min(255) << shift
    };
    let da = ((dst >> 24) & 0xFF) as f32;
    let alpha = ((a * 255.0 + da * inv).round() as u32).min(255) << 24;
    alpha | ch(16, c.0) | ch(8, c.1) | ch(0, c.2)
}

/// Turns the text mask in `px` into the finished premultiplied frame.
fn composite(px: &mut [u32], f: &Frame, shadow_dy: i32) {
    let (cx, cy, cw, ch) = f.card;
    let card = (cx as f32, cy as f32, cw as f32, ch as f32);
    let shadow = (cx as f32, (cy + shadow_dy) as f32, cw as f32, ch as f32);
    let r = f.radius as f32;
    let blur = (f.win_h - ch).max(8) as f32 / 2.0;
    let dot_c = (f.dot.0 as f32, f.dot.1 as f32);
    let dot_r = f.dot.2 as f32 / 2.0;
    let dot_col = f.tone.dot();
    for yy in 0..f.win_h {
        for xx in 0..f.win_w {
            let i = (yy * f.win_w + xx) as usize;
            let mask = ((px[i] >> 8) & 0xFF) as f32 / 255.0; // green channel
            let (x, y) = (xx as f32 + 0.5, yy as f32 + 0.5);
            let mut out = 0u32;
            // Soft shadow under the card.
            let sd = rounded_rect_sd(x, y, shadow, r);
            let t = (1.0 - (sd + 2.0) / blur).clamp(0.0, 1.0);
            out = over(out, Rgb(0, 0, 0), SHADOW_ALPHA * t * t);
            // Card and hairline border.
            let d = rounded_rect_sd(x, y, card, r);
            let cover = (0.5 - d).clamp(0.0, 1.0);
            if cover > 0.0 {
                out = over(out, CARD, CARD_ALPHA * cover);
                let ring = cover - (0.5 - (d + 1.0)).clamp(0.0, 1.0);
                out = over(out, Rgb(255, 255, 255), BORDER_ALPHA * ring);
                // Status dot.
                let dd = ((x - dot_c.0).powi(2) + (y - dot_c.1).powi(2)).sqrt() - dot_r;
                out = over(out, dot_col, (0.5 - dd).clamp(0.0, 1.0));
                // Text.
                out = over(out, INK, mask * cover);
            }
            px[i] = out;
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_UPDATE => update(hwnd, wparam.0),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER if wparam.0 == HIDE_TIMER => {
            *view() = View::default();
            update(hwnd, 0);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
