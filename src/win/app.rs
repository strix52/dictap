//! The app window: History, Dictionary and Settings pages in one dark window. Plain Win32 on
//! the ipc thread, created on demand and destroyed on close so it costs nothing while hidden.
//!
//! Everything is painted by hand into one DIB (anti-aliased rounded shapes, then ClearType
//! text on top); only text entry uses real EDIT controls, coloured to match. All geometry is
//! in 96-dpi units and scaled while painting. One `frame` pass both lays out and paints, so
//! hit-testing and child placement can't drift from what's on screen.
//!
//! The window carries WS_EX_DLGMODALFRAME: tiling managers such as komorebi leave those alone,
//! so it always opens as an ordinary floating window.
//!
//! Reads history through its own read-only connection; every change goes to core as an
//! `ActionRequest` and is answered by exactly one `ActionResult`. Nothing is assumed to have
//! worked: settings, dictionary, autostart and "Copied" change on screen only once the core
//! says so, and a failed reply puts the on-screen value back from the durable source.
//! Requests carry the window's generation, so a reply for a window that has since closed (and
//! been reopened) is drained and dropped rather than applied to the new one.

use super::ui;
use crate::event::{
    Action, ActionFailure, ActionRequest, ActionResult, ActionSuccess, Event, RequestId,
};
use crate::hotkey::Chord;
use crate::settings::Settings;
use crate::store::{self, Row, Store};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CLEARTYPE_QUALITY, ClientToScreen,
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CENTER, DT_EDITCONTROL, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW, EndPaint,
    GdiFlush, GetMonitorInfoW, GetTextExtentPoint32W, GetTextMetricsW, HBITMAP, HBRUSH, HDC, HFONT,
    HGDIOBJ, IntersectClipRect, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, SRCCOPY, ScreenToClient, SelectClipRgn, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{
    EM_SETCUEBANNER, EM_SETMARGINS, EM_SETSEL, SetWindowTheme, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT,
    TrackMouseEvent, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_DBLCLKS, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, EC_LEFTMARGIN, EC_RIGHTMARGIN, EN_CHANGE,
    EN_KILLFOCUS, EN_SETFOCUS, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_PASSWORD,
    ES_READONLY, ES_WANTRETURN, GetClientRect, GetCursorPos, GetSystemMetrics, HICON, ICON_BIG,
    ICON_SMALL, ICONINFO, IDC_ARROW, IMAGE_ICON, IsChild, IsIconic, KillTimer, LR_DEFAULTCOLOR,
    LoadCursorW, LoadImageW, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, PostMessageW,
    RegisterClassW, SM_CXICON, SM_CXSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, TPM_RETURNCMD, TPM_TOPALIGN, TrackPopupMenu, WM_APP, WM_CHAR, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO,
    WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_SETFOCUS, WM_SETFONT, WM_SETICON, WM_SIZE, WM_TIMER, WNDCLASSW,
    WS_CLIPCHILDREN, WS_EX_DLGMODALFRAME, WS_OVERLAPPEDWINDOW, WS_VSCROLL,
};
use windows::core::{HSTRING, PCSTR, PCWSTR, w};

const CLASS: PCWSTR = w!("dictap.app");
/// Posted (from any thread) when history rows change.
const WM_APP_CHANGED: u32 = WM_APP + 10;
/// Posted when an EDIT loses focus; wparam = its id. Deferred so the commit never runs
/// re-entrantly inside another handler.
const WM_APP_COMMIT: u32 = WM_APP + 11;
/// Posted (from the core thread) when an `ActionResult` is waiting in `RESULTS`.
const WM_APP_RESULT: u32 = WM_APP + 12;

/// Requests the window may have unanswered at once; more are refused (and reverted) locally.
const MAX_PENDING: usize = 16;
/// Replies parked for the UI thread. Pending is capped lower, so only stale generations can
/// ever fill it; the oldest are dropped first.
const RESULT_QUEUE: usize = 32;

/// Incremented every time the window is created.
static GENERATION: AtomicU64 = AtomicU64::new(0);
struct Replies {
    reserved: Vec<(RequestId, u64)>,
    queued: VecDeque<ActionResult>,
}
impl Replies {
    fn reserve(&mut self, id: RequestId, generation: u64) -> bool {
        if self.reserved.len() >= MAX_PENDING {
            return false;
        }
        self.reserved.push((id, generation));
        true
    }
    fn enqueue(&mut self, result: ActionResult) -> bool {
        if !self
            .reserved
            .contains(&(result.id, result.window_generation))
            || self.queued.iter().any(|r| r.id == result.id)
        {
            return false;
        }
        self.queued.push_back(result);
        debug_assert!(self.queued.len() <= RESULT_QUEUE);
        true
    }
    fn release(&mut self, id: RequestId) {
        self.reserved.retain(|&(r, _)| r != id);
    }
    fn clear(&mut self) {
        self.reserved.clear();
        self.queued.clear();
    }
}
static RESULTS: Mutex<Replies> = Mutex::new(Replies {
    reserved: Vec::new(),
    queued: VecDeque::new(),
});

/// What a pending request was for, so its reply can be applied or its optimism undone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Copy(i64),
    Settings,
    Dictionary,
    Autostart,
    Key,
    TestKey,
    Other,
}

struct PendingRequest {
    kind: Pending,
    draft: Option<String>,
}

fn same_kind(a: Pending, b: Pending) -> bool {
    std::mem::discriminant(&a) == std::mem::discriminant(&b)
}

fn draft(kind: Pending, edits: Edits) -> Option<String> {
    match kind {
        Pending::Settings => Some(format!(
            "{}\0{}",
            ui::get_text(edits.hotkey),
            ui::get_text(edits.language)
        )),
        Pending::Dictionary => Some(ui::get_text(edits.words)),
        Pending::Key => Some(ui::get_text(edits.key)),
        _ => None,
    }
}

fn request_draft(a: &App, kind: Pending) -> Option<String> {
    let field = draft(kind, a.edits);
    if kind == Pending::Settings {
        Some(format!(
            "{}\0{}",
            serde_json::to_string(&a.settings).unwrap_or_default(),
            field.unwrap_or_default()
        ))
    } else {
        field
    }
}

const SEARCH_TIMER: usize = 1;
const COPIED_TIMER: usize = 2;
const ARM_TIMER: usize = 3;
const NOTE_TIMER: usize = 4;
const RESULT_TIMER: usize = 5;
const LIMIT: usize = 5000;

const ID_SEARCH: usize = 100;
const ID_DETAIL: usize = 101;
const ID_HOTKEY: usize = 102;
const ID_LANGUAGE: usize = 103;
const ID_KEY: usize = 104;
const ID_WORDS: usize = 105;

// Palette: the overlay's dark neutrals, one accent.
#[derive(Clone, Copy, PartialEq)]
struct Rgb(u8, u8, u8);
const BG: Rgb = Rgb(0x1B, 0x1B, 0x1E);
const CARD: Rgb = Rgb(0x23, 0x23, 0x27);
const FIELD: Rgb = Rgb(0x2B, 0x2B, 0x30);
const HOVER: Rgb = Rgb(0x2C, 0x2C, 0x31);
const SELECT: Rgb = Rgb(0x35, 0x35, 0x3C);
const LINE: Rgb = Rgb(0x33, 0x33, 0x39);
const LINE_HI: Rgb = Rgb(0x48, 0x48, 0x50);
const INK: Rgb = Rgb(0xF5, 0xF5, 0xF7);
const INK2: Rgb = Rgb(0xAE, 0xAE, 0xB5);
const INK3: Rgb = Rgb(0x7A, 0x7A, 0x82);
const ACCENT: Rgb = Rgb(0x3D, 0x8B, 0xFF);
const RED: Rgb = Rgb(0xFF, 0x45, 0x3A);
const AMBER: Rgb = Rgb(0xFF, 0xB3, 0x40);
const GREEN: Rgb = Rgb(0x30, 0xD1, 0x58);

impl Rgb {
    fn px(self) -> u32 {
        u32::from(self.0) << 16 | u32::from(self.1) << 8 | u32::from(self.2)
    }
    fn cr(self) -> COLORREF {
        COLORREF(u32::from(self.0) | u32::from(self.1) << 8 | u32::from(self.2) << 16)
    }
}

// Layout, in 96-dpi units.
const WIN_W: i32 = 1000;
const WIN_H: i32 = 680;
const MIN_W: i32 = 780;
const MIN_H: i32 = 520;
const SIDE: f32 = 216.0;
const PAD: f32 = 28.0;
const BODY_TOP: f32 = 100.0;
const DAY_H: f32 = 34.0;
const ROW_H: f32 = 82.0;
const SET_ROW_H: f32 = 64.0;

// Segoe Fluent Icons / MDL2 Assets code points.
const G_HISTORY: char = '\u{E81C}';
const G_BOOK: char = '\u{E82D}';
const G_SETTINGS: char = '\u{E713}';
const G_COPY: char = '\u{E8C8}';
const G_DELETE: char = '\u{E74D}';
const G_SEARCH: char = '\u{E721}';
const G_RETRY: char = '\u{E72C}';
const G_CHECK: char = '\u{E73E}';

static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
static HWND_: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    History,
    Dictionary,
    Settings,
}

impl Page {
    pub fn from_index(i: usize) -> Page {
        match i {
            1 => Page::Dictionary,
            2 => Page::Settings,
            _ => Page::History,
        }
    }

    /// `--dictionary` or `--settings` on the command line; history otherwise.
    pub fn from_args() -> Page {
        let has = |a: &str| std::env::args().skip(1).any(|x| x == a);
        if has("--dictionary") {
            Page::Dictionary
        } else if has("--settings") {
            Page::Settings
        } else {
            Page::History
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Nav(Page),
    List,
    Row(usize),
    Thumb,
    Copy,
    Retry,
    Delete,
    ClearMenu,
    ClearYes,
    ClearNo,
    Sounds,
    Autostart,
    Keep(u32),
    SaveKey,
    TestKey,
    Import,
    Field(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

/// A rectangle in 96-dpi units.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct R {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn r(x: f32, y: f32, w: f32, h: f32) -> R {
    R { x, y, w, h }
}

impl R {
    fn right(self) -> f32 {
        self.x + self.w
    }
    fn bottom(self) -> f32 {
        self.y + self.h
    }
    fn inset(self, dx: f32, dy: f32) -> R {
        r(
            self.x + dx,
            self.y + dy,
            self.w - 2.0 * dx,
            self.h - 2.0 * dy,
        )
    }
    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    fn clip(self, c: R) -> R {
        let (x0, y0) = (self.x.max(c.x), self.y.max(c.y));
        let (x1, y1) = (self.right().min(c.right()), self.bottom().min(c.bottom()));
        r(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }
    fn within(self, c: R) -> bool {
        self.y >= c.y && self.bottom() <= c.bottom()
    }
}

#[derive(Clone, Copy)]
enum F {
    Body,
    Small,
    Label,
    Title,
    Icon,
    IconSmall,
}

struct Fonts {
    all: [HFONT; 6],
    /// Body line height, device pixels.
    body_h: i32,
}

impl Fonts {
    fn new(s: f32) -> Fonts {
        let face = ui::face();
        let variable = face == "Segoe UI Variable Text";
        let semibold = if variable {
            "Segoe UI Variable Text Semibold"
        } else {
            face
        };
        let display = if variable {
            "Segoe UI Variable Display Semibold"
        } else {
            face
        };
        let icons = if ui::installed("Segoe Fluent Icons") {
            "Segoe Fluent Icons"
        } else {
            "Segoe MDL2 Assets"
        };
        let px = |v: f32| (v * s).round() as i32;
        let all = [
            font(px(14.0), 400, face),
            font(px(12.0), 400, face),
            font(px(13.0), 600, semibold),
            font(px(24.0), 600, display),
            font(px(16.0), 400, icons),
            font(px(13.0), 400, icons),
        ];
        let body_h = line_height(all[0]);
        Fonts { all, body_h }
    }
    fn get(&self, f: F) -> HFONT {
        self.all[f as usize]
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for f in self.all {
            // SAFETY: fonts we created; no DC or control keeps them selected by now.
            let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
        }
    }
}

fn font(px: i32, weight: i32, face: &str) -> HFONT {
    // SAFETY: creates a GDI font the caller deletes.
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
            CLEARTYPE_QUALITY,
            0,
            &HSTRING::from(face),
        )
    }
}

fn line_height(f: HFONT) -> i32 {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: a scratch DC, released here.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, HGDIOBJ(f.0));
        let _ = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
    }
    tm.tmHeight
}

/// The back buffer: a top-down 32-bit DIB selected into the app's memory DC.
struct Canvas {
    w: i32,
    h: i32,
    bmp: HBITMAP,
    bits: *mut u32,
}

impl Canvas {
    fn new(dc: HDC, w: i32, h: i32) -> Option<Canvas> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: a w × h DIB; bits stays valid until the bitmap is deleted.
        let bmp =
            unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        Some(Canvas {
            w,
            h,
            bmp,
            bits: bits.cast(),
        })
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: deselected by the owner before dropping.
        let _ = unsafe { DeleteObject(HGDIOBJ(self.bmp.0)) };
    }
}

struct TextOp {
    text: Vec<u16>,
    font: HFONT,
    color: Rgb,
    rect: RECT,
    flags: DRAW_TEXT_FORMAT,
    clip: Option<RECT>,
}

/// Records geometry and, when given pixels, paints. Shapes go straight into the pixels;
/// text is queued and drawn by GDI afterwards, so it always lands on top.
struct Painter {
    s: f32,
    px: *mut u32,
    w: i32,
    h: i32,
    clip: Option<R>,
    texts: Vec<TextOp>,
}

impl Painter {
    /// Snaps a logical rect to device pixel edges.
    fn dev(&self, a: R) -> (f32, f32, f32, f32) {
        let s = self.s;
        (
            (a.x * s).round(),
            (a.y * s).round(),
            ((a.x + a.w) * s).round(),
            ((a.y + a.h) * s).round(),
        )
    }

    fn dev_i(&self, a: R) -> RECT {
        let (x0, y0, x1, y1) = self.dev(a);
        RECT {
            left: x0 as i32,
            top: y0 as i32,
            right: x1 as i32,
            bottom: y1 as i32,
        }
    }

    fn fill(&mut self, a: R, radius: f32, c: Rgb) {
        self.shape(a, radius, c, 1.0, None);
    }

    /// A rounded rect, or with `ring` only a band that wide inside its edge.
    fn shape(&mut self, a: R, radius: f32, c: Rgb, alpha: f32, ring: Option<f32>) {
        if self.px.is_null() || a.w <= 0.0 || a.h <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = self.dev(a);
        let rad = (radius * self.s).min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
        let ring = ring.map(|w| (w * self.s).max(1.0));
        let (mut cx0, mut cy0, mut cx1, mut cy1) = (0, 0, self.w, self.h);
        if let Some(c) = self.clip {
            let c = self.dev_i(c);
            (cx0, cy0, cx1, cy1) = (
                c.left.max(0),
                c.top.max(0),
                c.right.min(self.w),
                c.bottom.min(self.h),
            );
        }
        let (xa, xb) = ((x0 as i32).max(cx0), (x1 as i32).min(cx1));
        let (ya, yb) = ((y0 as i32).max(cy0), (y1 as i32).min(cy1));
        // SAFETY: the canvas outlives the painter; indices stay inside w × h.
        let px = unsafe { std::slice::from_raw_parts_mut(self.px, (self.w * self.h) as usize) };
        let (cx, cy, hw, hh) = (
            (x0 + x1) / 2.0,
            (y0 + y1) / 2.0,
            (x1 - x0) / 2.0,
            (y1 - y0) / 2.0,
        );
        for y in ya..yb {
            let fy = y as f32 + 0.5;
            // Away from the corners' rows only the straight edges matter.
            let band = rad <= 0.0 || (fy - y0 > rad && y1 - fy > rad);
            for x in xa..xb {
                let fx = x as f32 + 0.5;
                let d = if band {
                    (x0 - fx).max(fx - x1).max(y0 - fy).max(fy - y1)
                } else {
                    let qx = (fx - cx).abs() - (hw - rad);
                    let qy = (fy - cy).abs() - (hh - rad);
                    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
                    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - rad
                };
                let mut cover = (0.5 - d).clamp(0.0, 1.0);
                if let Some(w) = ring {
                    cover -= (0.5 - d - w).clamp(0.0, 1.0);
                }
                if cover > 0.0 {
                    let i = (y * self.w + x) as usize;
                    px[i] = blend(px[i], c, cover * alpha);
                }
            }
        }
    }

    fn text(&mut self, s: &str, font: HFONT, color: Rgb, a: R, flags: DRAW_TEXT_FORMAT) {
        if self.px.is_null() || s.is_empty() {
            return;
        }
        let clip = self.clip.map(|c| self.dev_i(c));
        self.texts.push(TextOp {
            text: s.encode_utf16().collect(),
            font,
            color,
            rect: self.dev_i(a),
            flags: flags | DT_NOPREFIX,
            clip,
        });
    }
}

fn blend(dst: u32, c: Rgb, a: f32) -> u32 {
    if a >= 1.0 {
        return c.px();
    }
    let k = (a * 256.0) as u32;
    let inv = 256 - k;
    let ch = |shift: u32, v: u8| ((u32::from(v) * k + ((dst >> shift) & 0xFF) * inv) >> 8) << shift;
    ch(16, c.0) | ch(8, c.1) | ch(0, c.2)
}

const LINE1: DRAW_TEXT_FORMAT =
    DRAW_TEXT_FORMAT(DT_SINGLELINE.0 | DT_VCENTER.0 | DT_END_ELLIPSIS.0);

#[derive(Clone, Copy)]
struct Edits {
    search: HWND,
    detail: HWND,
    hotkey: HWND,
    language: HWND,
    key: HWND,
    words: HWND,
}

impl Edits {
    fn all(&self) -> [(usize, HWND); 6] {
        [
            (ID_SEARCH, self.search),
            (ID_DETAIL, self.detail),
            (ID_HOTKEY, self.hotkey),
            (ID_LANGUAGE, self.language),
            (ID_KEY, self.key),
            (ID_WORDS, self.words),
        ]
    }
    fn by_id(&self, id: usize) -> Option<HWND> {
        self.all()
            .into_iter()
            .find(|&(i, _)| i == id)
            .map(|(_, h)| h)
    }
}

/// Brushes for WM_CTLCOLOR*, readable while the app state is borrowed.
#[derive(Clone, Copy)]
struct Paint {
    field: HBRUSH,
    card: HBRUSH,
    detail: HWND,
    words: HWND,
}

enum Item {
    Day(String),
    Row(usize),
}

/// A pending bulk delete, shown as a banner until confirmed or dismissed.
struct Clear {
    cutoff: i64,
    count: i64,
    what: &'static str,
}

struct App {
    hwnd: HWND,
    s: f32,
    dc: HDC,
    canvas: Option<Canvas>,
    fonts: Fonts,
    edits: Edits,
    icons: [HICON; 2],
    page: Page,
    hits: Vec<(R, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    tracking: bool,
    /// Thumb drag: mouse y and scroll at the start.
    drag: Option<(f32, f32, bool)>,
    /// Where each EDIT should be (device pixels), and where it was last put.
    want: Vec<(HWND, Option<RECT>)>,
    placed: Vec<(HWND, Option<RECT>)>,

    store: Option<Store>,
    rows: Vec<Row>,
    items: Vec<(f32, f32, Item)>,
    list_h: f32,
    list_view: R,
    scroll: f32,
    sel: Option<usize>,
    total: i64,
    copied: bool,
    armed: bool,
    clear: Option<Clear>,
    detail_shown: Option<String>,

    settings: Settings,
    autostart: bool,
    words: Vec<String>,
    hotkey_error: bool,
    /// A message about the key and whether it is good news.
    key_note: Option<(String, bool)>,
    generation: u64,
    pending: HashMap<RequestId, PendingRequest>,
    action_note: Option<String>,
    set_view: R,
    set_h: f32,
    set_scroll: f32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static PAINT: Cell<Option<Paint>> = const { Cell::new(None) };
}

/// Runs `f` on the app state unless it's absent or already borrowed further up the stack
/// (a message sent from inside another handler).
fn with<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP.with(|a| {
        let mut g = a.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

fn hwnd() -> Option<HWND> {
    let h = HWND_.load(Ordering::Acquire);
    (h != 0).then_some(HWND(h as *mut _))
}

/// Sends `action` to the core and remembers what it was for. Returns false if it could not be
/// sent (too many unanswered requests, or the core is gone); the caller then undoes any
/// optimistic change.
fn request(action: Action, kind: Pending) -> bool {
    let Some(Some((id, window_generation))) = with(|a| {
        if a.pending.len() >= MAX_PENDING || a.pending.values().any(|p| same_kind(p.kind, kind)) {
            return None;
        }
        let id = RequestId::next();
        if !RESULTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reserve(id, a.generation)
        {
            return None;
        }
        a.pending.insert(
            id,
            PendingRequest {
                kind,
                draft: request_draft(a, kind),
            },
        );
        Some((id, a.generation))
    }) else {
        return false;
    };
    let sent = super::ipc::try_send(Event::Ui(ActionRequest {
        id,
        window_generation,
        action,
    }));
    if !sent {
        with(|a| a.pending.remove(&id));
        RESULTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(id);
    } else {
        start_timer(RESULT_TIMER, 200);
    }
    sent
}

/// From the core thread: queue its answer for the window. Dropped if no window is open.
pub fn reply(result: ActionResult) {
    let Some(h) = hwnd() else { return };
    {
        let mut q = RESULTS.lock().unwrap_or_else(PoisonError::into_inner);
        if !q.enqueue(result) {
            return;
        }
    }
    // SAFETY: a stale handle just fails; create() clears the queue for the next window.
    let _ = unsafe { PostMessageW(Some(h), WM_APP_RESULT, WPARAM(0), LPARAM(0)) };
}

fn drain_results() {
    let batch: Vec<ActionResult> = {
        let mut replies = RESULTS.lock().unwrap_or_else(PoisonError::into_inner);
        let batch: Vec<_> = replies.queued.drain(..).collect();
        for r in &batch {
            replies.release(r.id);
        }
        batch
    };
    for r in batch {
        apply_result(r);
    }
    refresh();
}

fn set_key_note(text: impl Into<String>, good: bool) {
    with(|a| a.key_note = Some((text.into(), good)));
    start_timer(NOTE_TIMER, 5000);
}

fn apply_result(r: ActionResult) {
    let Some(Some(kind)) = with(|a| {
        (a.generation == r.window_generation)
            .then(|| a.pending.remove(&r.id))
            .flatten()
    }) else {
        return;
    };
    let unchanged = with(|a| request_draft(a, kind.kind) == kind.draft).unwrap_or(false);
    if let Err(ActionFailure(why)) = &r.result {
        with(|a| a.action_note = Some(why.clone()));
        start_timer(NOTE_TIMER, 5000);
    }
    match (kind.kind, r.result) {
        (Pending::Copy(row), Ok(_)) => {
            with(|a| {
                if a.sel
                    .and_then(|s| a.rows.get(s))
                    .is_some_and(|r| r.id == row)
                {
                    a.copied = true;
                }
            });
            start_timer(COPIED_TIMER, 1400);
        }
        (Pending::Settings, Err(_)) if unchanged => revert(Pending::Settings),
        (Pending::Dictionary, Err(_)) if unchanged => revert(Pending::Dictionary),
        (Pending::Autostart, Err(_)) => revert(Pending::Autostart),
        (Pending::Key, Ok(ActionSuccess::KeySaved { test_started: true })) => {
            if unchanged && let Some(edit) = with(|a| a.edits.key) {
                ui::set_text(edit, "");
            }
            set_key_note("Saved — checking it with Gemini…", true);
        }
        (Pending::Key, Ok(_)) => {
            if unchanged && let Some(edit) = with(|a| a.edits.key) {
                ui::set_text(edit, "");
            }
            set_key_note("Saved — not checked yet", true);
        }
        (Pending::Key, Err(ActionFailure(why))) => set_key_note(why, false),
        (Pending::TestKey, Ok(_)) => set_key_note("Gemini key works", true),
        (Pending::TestKey, Err(ActionFailure(why))) => set_key_note(why, false),
        _ => {}
    }
    // Changes made while a save was pending are coalesced after its acknowledgment.
    if !unchanged {
        match kind.kind {
            Pending::Settings => {
                if let Some(newer) = with(|a| {
                    let mut newer = a.settings.clone();
                    if let Some(c) = Chord::parse(&ui::get_text(a.edits.hotkey)) {
                        newer.hotkey = c.to_string();
                    }
                    newer.language = ui::get_text(a.edits.language).trim().to_string();
                    a.settings = newer.clone();
                    newer
                }) {
                    request(Action::SaveSettings(newer), Pending::Settings);
                }
            }
            Pending::Dictionary => {
                with(|a| a.words.clear());
                commit(ID_WORDS);
            }
            _ => {}
        }
    }
}

/// Puts a field back to what is actually stored, after a refused or failed change.
fn revert(kind: Pending) {
    match kind {
        Pending::Settings => {
            let Some(Some(path)) = with(|_| PATHS.get().map(|p| p.0.clone())) else {
                return;
            };
            let s = Settings::load(&path);
            let Some(edits) = with(|a| {
                a.settings = s.clone();
                a.hotkey_error = false;
                a.edits
            }) else {
                return;
            };
            ui::set_text(edits.hotkey, &s.hotkey);
            ui::set_text(edits.language, &s.language);
        }
        Pending::Dictionary => {
            let Some((words, edit)) = with(|a| {
                let words = a
                    .store
                    .as_ref()
                    .and_then(|s| s.dictionary().ok())
                    .unwrap_or_else(|| a.words.clone());
                a.words = words.clone();
                (words, a.edits.words)
            }) else {
                return;
            };
            ui::set_text(edit, &words.join("\r\n"));
        }
        Pending::Autostart => {
            with(|a| a.autostart = crate::autostart::enabled());
        }
        _ => {}
    }
    refresh();
}

/// Settings file and database paths.
pub fn init(settings: PathBuf, db: PathBuf) {
    let _ = PATHS.set((settings, db));
}

/// From any thread: refresh the history if the window is open.
pub fn changed() {
    if let Some(h) = hwnd() {
        // SAFETY: a stale handle just fails.
        let _ = unsafe { PostMessageW(Some(h), WM_APP_CHANGED, WPARAM(0), LPARAM(0)) };
    }
}

/// Shows (creating if needed) and focuses the window on `page`. ipc thread only.
pub fn show(page: Page) {
    if let Some(h) = hwnd() {
        // SAFETY: our own window.
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
        go(page);
        return;
    }
    if let Err(e) = create(page) {
        log::error!("app window: {e}");
    }
}

/// Keyboard handling that has to happen before dispatch: Tab between fields, Escape and
/// Enter in fields, Ctrl+F. Call from the ipc message loop.
pub fn pre_translate(msg: &MSG) -> bool {
    let Some(app) = hwnd() else {
        return false;
    };
    // SAFETY: plain window queries.
    let ours = msg.hwnd == app || unsafe { IsChild(app, msg.hwnd) }.as_bool();
    if !ours || msg.message != WM_KEYDOWN {
        return false;
    }
    let Some((edits, page)) = with(|a| (a.edits, a.page)) else {
        return false;
    };
    // SAFETY: plain focus and key state queries.
    let (focus, ctrl, shift) = unsafe {
        (
            GetFocus(),
            GetKeyState(VK_CONTROL.0 as i32) < 0,
            GetKeyState(VK_SHIFT.0 as i32) < 0,
        )
    };
    let key = msg.wParam.0 as u16;
    let focus_app = || {
        // SAFETY: focusing our own window.
        let _ = unsafe { SetFocus(Some(app)) };
    };
    match key {
        k if k == VK_TAB.0 => {
            let order: Vec<HWND> = match page {
                Page::History => vec![app, edits.search, edits.detail],
                Page::Dictionary => vec![edits.words, app],
                Page::Settings => vec![app, edits.hotkey, edits.language, edits.key],
            };
            let at = order.iter().position(|&h| h == focus).unwrap_or(0);
            let n = order.len();
            let next = if shift {
                (at + n - 1) % n
            } else {
                (at + 1) % n
            };
            // SAFETY: focusing our own window or child.
            let _ = unsafe { SetFocus(Some(order[next])) };
            if order[next] != app {
                ui::send(order[next], EM_SETSEL, 0, -1);
            }
            true
        }
        k if k == VK_ESCAPE.0 => {
            if focus == edits.search && !ui::get_text(edits.search).is_empty() {
                ui::set_text(edits.search, "");
            } else if focus != app {
                focus_app();
            } else if with(|a| a.clear.take().is_some() || std::mem::take(&mut a.armed))
                == Some(true)
            {
                refresh();
            } else {
                // SAFETY: our own window.
                let _ = unsafe { PostMessageW(Some(app), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
            true
        }
        k if k == VK_RETURN.0 && focus == edits.key => {
            save_key();
            true
        }
        k if (k == VK_RETURN.0 || k == VK_DOWN.0) && focus == edits.search => {
            focus_app();
            if with(|a| a.sel.is_none() && !a.rows.is_empty()) == Some(true) {
                select(Some(0));
            }
            true
        }
        k if k == VK_RETURN.0 && (focus == edits.hotkey || focus == edits.language) => {
            focus_app();
            true
        }
        k if k == u16::from(b'F') && ctrl && page == Page::History => {
            // SAFETY: focusing our own child.
            let _ = unsafe { SetFocus(Some(edits.search)) };
            ui::send(edits.search, EM_SETSEL, 0, -1);
            true
        }
        _ => false,
    }
}

fn dark_mode() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Undocumented but long-stable uxtheme exports (ordinal 135 SetPreferredAppMode,
        // 136 FlushMenuThemes) that give menus and scrollbars their dark look.
        // SAFETY: looked up by ordinal; each is called with its known signature.
        unsafe {
            let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else {
                return;
            };
            if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
                let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                set(2); // ForceDark
            }
            if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
                let flush: extern "system" fn() = std::mem::transmute(f);
                flush();
            }
        }
    });
}

/// The app icon from the exe's resources, or a drawn stand-in when it was built without them.
fn app_icon(n: i32) -> HICON {
    // SAFETY: loads icon resource 1 of our own module at the requested size.
    let loaded = unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(
            Some(module.into()),
            // MAKEINTRESOURCE(1)
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            n,
            n,
            LR_DEFAULTCOLOR,
        )
    };
    match loaded {
        Ok(h) if !h.is_invalid() => HICON(h.0),
        _ => drawn_icon(n),
    }
}

/// A dark tile with the overlay's red dot and three bars.
fn drawn_icon(n: i32) -> HICON {
    let mut px = vec![0u32; (n * n) as usize];
    let s = n as f32 / 32.0;
    let mut put = |x0: f32, y0: f32, x1: f32, y1: f32, rad: f32, c: Rgb| {
        let (cx, cy, hw, hh) = (
            (x0 + x1) / 2.0,
            (y0 + y1) / 2.0,
            (x1 - x0) / 2.0,
            (y1 - y0) / 2.0,
        );
        let rad = rad.min(hw).min(hh);
        for y in 0..n {
            for x in 0..n {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let qx = (fx - cx).abs() - (hw - rad);
                let qy = (fy - cy).abs() - (hh - rad);
                let (ox, oy) = (qx.max(0.0), qy.max(0.0));
                let d = (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - rad;
                let a = (0.5 - d).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                // Premultiplied "over".
                let i = (y * n + x) as usize;
                let d0 = px[i];
                let inv = 1.0 - a;
                let ch = |shift: u32, v: u8| {
                    ((f32::from(v) * a + ((d0 >> shift) & 0xFF) as f32 * inv).round() as u32)
                        << shift
                };
                let al = ((255.0 * a + (d0 >> 24) as f32 * inv).round() as u32) << 24;
                px[i] = al | ch(16, c.0) | ch(8, c.1) | ch(0, c.2);
            }
        }
    };
    put(
        1.0 * s,
        1.0 * s,
        31.0 * s,
        31.0 * s,
        8.0 * s,
        Rgb(0x26, 0x26, 0x2B),
    );
    put(6.0 * s, 12.0 * s, 14.0 * s, 20.0 * s, 4.0 * s, RED);
    for (i, h) in [8.0, 16.0, 10.0].iter().enumerate() {
        let x = (17.0 + i as f32 * 4.5) * s;
        put(
            x,
            (16.0 - h / 2.0) * s,
            x + 2.6 * s,
            (16.0 + h / 2.0) * s,
            1.3 * s,
            INK,
        );
    }
    let mask = vec![0u8; ((n + 15) / 16 * 2 * n) as usize];
    // SAFETY: bitmaps sized to the buffers; deleted after the icon copies them.
    unsafe {
        let color = CreateBitmap(n, n, 1, 32, Some(px.as_ptr().cast()));
        let mask = CreateBitmap(n, n, 1, 1, Some(mask.as_ptr().cast()));
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

fn create(page: Page) -> windows::core::Result<()> {
    let Some((settings_path, db)) = PATHS.get() else {
        return Ok(());
    };
    dark_mode();
    let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
    let ex = WS_EX_DLGMODALFRAME;
    // SAFETY: class registration (a repeat fails harmlessly) and window creation.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            ex,
            CLASS,
            w!("dictap"),
            style,
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
    // Dark title bar in the window's own colour, so it reads as one surface.
    // SAFETY: plain attribute writes on our window; unsupported ones just fail.
    unsafe {
        let on: i32 = 1;
        let set = |attr, v: &u32| {
            let _ = DwmSetWindowAttribute(hwnd, attr, std::ptr::from_ref(v).cast(), 4);
        };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&on).cast(),
            4,
        );
        set(DWMWA_CAPTION_COLOR, &BG.cr().0);
        set(DWMWA_TEXT_COLOR, &INK2.cr().0);
        set(DWMWA_BORDER_COLOR, &LINE.cr().0);
    }

    // Centred on the monitor under the cursor, at that monitor's scale.
    // SAFETY: plain queries and a resize of our own window.
    let dpi = unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96, 96);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let work = mi.rcWork;
        let mut rc = ui::rect(0, 0, ui::px(WIN_W, dx), ui::px(WIN_H, dx));
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, ex, dx);
        let (w, h) = (
            (rc.right - rc.left).min(work.right - work.left),
            (rc.bottom - rc.top).min(work.bottom - work.top),
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            work.left + (work.right - work.left - w) / 2,
            work.top + (work.bottom - work.top - h) / 2,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        ui::dpi(hwnd)
    };
    let s = dpi as f32 / 96.0;
    let fonts = Fonts::new(s);
    let body = fonts.get(F::Body);

    let settings = Settings::load(settings_path);
    let edit = w!("EDIT");
    let single = ES_AUTOHSCROLL as u32;
    let multi = (ES_MULTILINE | ES_AUTOVSCROLL) as u32 | WS_VSCROLL.0;
    let mk = |text: &str, style: u32, id: usize, cue: &str| {
        let h = ui::child(hwnd, edit, text, style, 0, id, body);
        if !cue.is_empty() {
            let cue = HSTRING::from(cue);
            ui::send(h, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
        }
        // SAFETY: hidden until the first layout places it.
        let _ = unsafe { ShowWindow(h, SW_HIDE) };
        h
    };
    let words = crate::store::Store::open_read(db)
        .and_then(|s| s.dictionary())
        .unwrap_or_default();
    let edits = Edits {
        search: mk("", single, ID_SEARCH, "Search"),
        detail: mk("", multi | ES_READONLY as u32, ID_DETAIL, ""),
        hotkey: mk(&settings.hotkey, single, ID_HOTKEY, "Ctrl+Win"),
        language: mk(&settings.language, single, ID_LANGUAGE, "Auto-detect"),
        key: mk("", single | ES_PASSWORD as u32, ID_KEY, "Paste a new key"),
        words: mk(
            &words.join("\r\n"),
            multi | ES_WANTRETURN as u32,
            ID_WORDS,
            "",
        ),
    };
    for h in [edits.detail, edits.words] {
        // SAFETY: theming our own controls; dark scrollbars.
        let _ = unsafe { SetWindowTheme(h, w!("DarkMode_Explorer"), None) };
        ui::send(
            h,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
            0,
        );
    }
    // SAFETY: brushes deleted on WM_DESTROY.
    PAINT.with(|p| {
        p.set(Some(Paint {
            field: unsafe { CreateSolidBrush(FIELD.cr()) },
            card: unsafe { CreateSolidBrush(CARD.cr()) },
            detail: edits.detail,
            words: edits.words,
        }))
    });
    let icons = [
        // SAFETY: plain metric queries.
        app_icon(unsafe { GetSystemMetrics(SM_CXICON) }),
        app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) }),
    ];
    ui::send(hwnd, WM_SETICON, ICON_BIG as usize, icons[0].0 as isize);
    ui::send(hwnd, WM_SETICON, ICON_SMALL as usize, icons[1].0 as isize);

    let store = match Store::open_read(db) {
        Ok(s) => Some(s),
        Err(e) => {
            log::error!("history read connection: {e}");
            None
        }
    };
    // A new window is a new generation: replies still in flight for the old one are dropped.
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    RESULTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    // SAFETY: a memory DC for measuring and painting, deleted on WM_DESTROY.
    let dc = unsafe { CreateCompatibleDC(None) };
    // SAFETY: plain setup of our DC.
    unsafe { SetBkMode(dc, TRANSPARENT) };
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            hwnd,
            s,
            dc,
            canvas: None,
            fonts,
            edits,
            icons,
            page,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            tracking: false,
            drag: None,
            want: Vec::new(),
            placed: edits.all().iter().map(|&(_, h)| (h, None)).collect(),
            store,
            rows: Vec::new(),
            items: Vec::new(),
            list_h: 0.0,
            list_view: R::default(),
            scroll: 0.0,
            sel: None,
            total: 0,
            copied: false,
            armed: false,
            clear: None,
            detail_shown: None,
            settings,
            autostart: crate::autostart::enabled(),
            words,
            hotkey_error: false,
            key_note: None,
            generation,
            pending: HashMap::new(),
            action_note: None,
            set_view: R::default(),
            set_h: 0.0,
            set_scroll: 0.0,
        })
    });
    HWND_.store(hwnd.0 as isize, Ordering::Release);
    reload(true);
    // SAFETY: showing and focusing our own window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    go(page);
    Ok(())
}

/// Switches page and puts the focus somewhere sensible.
fn go(page: Page) {
    commit(ID_WORDS);
    let Some(edits) = with(|a| {
        if a.page != page {
            a.page = page;
            a.hover = None;
            a.set_scroll = 0.0;
        }
        a.edits
    }) else {
        return;
    };
    refresh();
    let Some(app) = hwnd() else { return };
    let target = match page {
        Page::Dictionary => edits.words,
        _ => app,
    };
    // SAFETY: focusing our own window or child.
    let _ = unsafe { SetFocus(Some(target)) };
}

/// Re-lays out, moves the EDITs to match and repaints. Never call with the state borrowed.
fn refresh() {
    let Some((moves, detail)) = with(|a| {
        a.frame(None);
        let moves: Vec<(HWND, Option<RECT>)> = a
            .want
            .iter()
            .zip(&a.placed)
            .filter(|(w, p)| w.1 != p.1)
            .map(|(w, _)| *w)
            .collect();
        a.placed = a.want.clone();
        let detail = a.detail_text();
        let detail = (a.detail_shown.as_ref() != Some(&detail)).then(|| {
            a.detail_shown = Some(detail.clone());
            (a.edits.detail, detail)
        });
        (moves, detail)
    }) else {
        return;
    };
    for (h, want) in moves {
        // SAFETY: moving or hiding our own children.
        unsafe {
            match want {
                Some(rc) => {
                    let _ = SetWindowPos(
                        h,
                        None,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
                None => {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
        }
    }
    if let Some((h, text)) = detail {
        ui::set_text(h, &text);
    }
    invalidate();
}

fn invalidate() {
    if let Some(h) = hwnd() {
        // SAFETY: repaint our own window.
        let _ = unsafe { InvalidateRect(Some(h), None, false) };
    }
}

/// Re-runs the search, keeping the selection (or its position, after a delete).
fn reload(first: bool) {
    let Some(search) = with(|a| a.edits.search) else {
        return;
    };
    let query = ui::get_text(search);
    with(|a| {
        let keep = a.sel.and_then(|i| a.rows.get(i)).map(|r| r.id);
        let at = a.sel;
        let Some(st) = &a.store else { return };
        a.rows = st.search(&query, LIMIT).unwrap_or_else(|e| {
            log::error!("history search: {e}");
            Vec::new()
        });
        a.total = st.count_before(i64::MAX).unwrap_or(0);
        a.group();
        a.sel = keep
            .and_then(|id| a.rows.iter().position(|r| r.id == id))
            .or_else(|| at.map(|i| i.min(a.rows.len().saturating_sub(1))))
            .or((first && !a.rows.is_empty()).then_some(0))
            .filter(|&i| i < a.rows.len());
        if first {
            a.scroll = 0.0;
        }
        a.clamp_scroll();
    });
    refresh();
}

fn select(i: Option<usize>) {
    with(|a| {
        if a.sel != i {
            a.sel = i;
            a.armed = false;
            a.copied = false;
        }
        a.reveal();
    });
    refresh();
}

fn start_timer(id: usize, ms: u32) {
    if let Some(h) = hwnd() {
        // SAFETY: timer on our own window.
        unsafe { SetTimer(Some(h), id, ms, None) };
    }
}

fn copy() {
    let Some(id) = with(|a| {
        a.sel
            .and_then(|i| a.rows.get(i))
            .filter(|r| !r.text.is_empty())
            .map(|r| r.id)
    })
    .flatten() else {
        return;
    };
    // "Copied" appears only when the clipboard worker reports success.
    request(Action::Copy(id), Pending::Copy(id));
}

fn delete() {
    let Some((id, now)) = with(|a| {
        let id = a.sel.and_then(|i| a.rows.get(i))?.id;
        let now = a.armed;
        a.armed = !now;
        Some((id, now))
    })
    .flatten() else {
        return;
    };
    if now {
        request(Action::Delete(id), Pending::Other);
    } else {
        start_timer(ARM_TIMER, 3000);
    }
    refresh();
}

fn clear_menu() {
    let Some((app, at, st_ok)) = with(|a| {
        let b = a.hits.iter().find(|h| h.1 == Hit::ClearMenu).map(|h| h.0)?;
        Some((
            a.hwnd,
            a.to_screen(b.x, b.bottom() + 4.0),
            a.store.is_some(),
        ))
    })
    .flatten() else {
        return;
    };
    if !st_ok {
        return;
    }
    const DAY: i64 = 86_400_000;
    let choices: [(&str, &'static str, i64); 4] = [
        ("Older than 7 days", "older than 7 days", 7),
        ("Older than 30 days", "older than 30 days", 30),
        ("Older than 90 days", "older than 90 days", 90),
        ("All history", "", 0),
    ];
    // SAFETY: a popup menu owned and destroyed here, tracked modally on our window.
    let cmd = unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        for (i, (label, _, _)) in choices.iter().enumerate() {
            if i == 3 {
                let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
            }
            let _ = AppendMenuW(m, MF_STRING, i + 1, &HSTRING::from(*label));
        }
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_TOPALIGN, at.x, at.y, None, app, None);
        let _ = DestroyMenu(m);
        cmd.0 as usize
    };
    let Some(&(_, what, days)) = cmd.checked_sub(1).and_then(|i| choices.get(i)) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let cutoff = if days == 0 {
        i64::MAX
    } else {
        now - days * DAY
    };
    with(|a| {
        let count = a
            .store
            .as_ref()
            .and_then(|s| s.count_before(cutoff).ok())
            .unwrap_or(0);
        a.clear = Some(Clear {
            cutoff,
            count,
            what,
        });
    });
    refresh();
}

fn save_settings(f: impl FnOnce(&mut Settings)) {
    let changed = with(|a| {
        let mut s = a.settings.clone();
        f(&mut s);
        (s != a.settings).then(|| {
            a.settings = s.clone();
            s
        })
    })
    .flatten();
    if let Some(s) = changed
        && !request(Action::SaveSettings(s), Pending::Settings)
    {
        with(|a| a.action_note = Some("Save pending — newer edits will follow".into()));
    }
    refresh();
}

fn save_key() {
    let Some(key_edit) = with(|a| a.edits.key) else {
        return;
    };
    let key = ui::get_text(key_edit);
    if key.trim().is_empty() {
        return;
    }
    if !request(Action::SetApiKey(key.trim().to_string()), Pending::Key) {
        set_key_note("Couldn't send the key — try again", false);
    }
    refresh();
}

/// Saves a text field once the user leaves it.
fn commit(id: usize) {
    let Some(edits) = with(|a| a.edits) else {
        return;
    };
    match id {
        ID_HOTKEY => {
            let text = ui::get_text(edits.hotkey);
            match Chord::parse(&text) {
                Some(c) => {
                    let norm = c.to_string();
                    if norm != text {
                        ui::set_text(edits.hotkey, &norm);
                    }
                    with(|a| a.hotkey_error = false);
                    save_settings(|s| s.hotkey = norm);
                }
                None => {
                    with(|a| a.hotkey_error = true);
                    refresh();
                }
            }
        }
        ID_LANGUAGE => {
            let lang = ui::get_text(edits.language).trim().to_string();
            save_settings(|s| s.language = lang);
        }
        ID_WORDS => {
            let words: Vec<String> = ui::get_text(edits.words)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let changed = with(|a| {
                let c = a.words != words;
                a.words = words.clone();
                c
            });
            if changed == Some(true) && !request(Action::SetDictionary(words), Pending::Dictionary)
            {
                with(|a| a.action_note = Some("Save pending — newer edits will follow".into()));
            }
        }
        _ => {}
    }
}

fn click(hit: Hit) {
    match hit {
        Hit::Nav(p) => go(p),
        Hit::Copy => copy(),
        Hit::Delete => delete(),
        Hit::Retry => {
            if let Some(Some(id)) = with(|a| {
                a.sel
                    .and_then(|i| a.rows.get(i))
                    .filter(|r| r.audio_path.is_some())
                    .map(|r| r.id)
            }) {
                request(Action::Retry(id), Pending::Other);
            }
        }
        Hit::ClearMenu => clear_menu(),
        Hit::ClearYes => {
            if let Some(Some(c)) = with(|a| a.clear.take())
                && c.count > 0
            {
                request(Action::ClearBefore(c.cutoff), Pending::Other);
            }
            refresh();
        }
        Hit::ClearNo => {
            with(|a| a.clear = None);
            refresh();
        }
        Hit::Sounds => save_settings(|s| s.sounds = !s.sounds),
        Hit::Keep(d) => save_settings(|s| s.keep_days = d),
        Hit::Autostart => {
            if let Some(on) = with(|a| {
                a.autostart = !a.autostart;
                a.autostart
            }) && !request(Action::SetAutostart(on), Pending::Autostart)
            {
                revert(Pending::Autostart);
            }
            refresh();
        }
        Hit::SaveKey => save_key(),
        Hit::TestKey => {
            if !request(Action::TestKey, Pending::TestKey) {
                set_key_note("Couldn't start the check — try again", false);
                refresh();
            }
        }
        Hit::Import => {
            request(Action::ImportOpenWhispr, Pending::Other);
        }
        Hit::List | Hit::Row(_) | Hit::Thumb | Hit::Field(_) => {}
    }
}

fn thousands(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// "Today", "Yesterday", a weekday within the week, else the date.
fn day_label(ms: i64, now: i64) -> String {
    let key = |t: &windows::Win32::Foundation::SYSTEMTIME| (t.wYear, t.wMonth, t.wDay);
    let (Some(t), Some(today)) = (ui::local(ms), ui::local(now)) else {
        return String::new();
    };
    if key(&t) == key(&today) {
        return "Today".into();
    }
    if ui::local(now - 86_400_000).is_some_and(|y| key(&y) == key(&t)) {
        return "Yesterday".into();
    }
    let month = MONTHS[usize::from(t.wMonth.clamp(1, 12)) - 1];
    if now - ms < 6 * 86_400_000 {
        WEEKDAYS[usize::from(t.wDayOfWeek % 7)].into()
    } else if t.wYear == today.wYear {
        format!("{} {month}", t.wDay)
    } else {
        format!("{} {month} {}", t.wDay, t.wYear)
    }
}

fn clock(ms: i64) -> String {
    ui::local(ms).map_or(String::new(), |t| {
        format!("{:02}:{:02}", t.wHour, t.wMinute)
    })
}

fn duration(ms: i64) -> String {
    let secs = (ms as f64 / 1000.0).round() as i64;
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn preview(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if out.len() > 400 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

impl App {
    fn client(&self) -> (f32, f32) {
        let mut rc = RECT::default();
        // SAFETY: plain query.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rc) };
        (rc.right as f32 / self.s, rc.bottom as f32 / self.s)
    }

    fn to_screen(&self, x: f32, y: f32) -> POINT {
        let mut pt = POINT {
            x: (x * self.s) as i32,
            y: (y * self.s) as i32,
        };
        // SAFETY: plain conversion.
        let _ = unsafe { ClientToScreen(self.hwnd, &mut pt) };
        pt
    }

    /// Text width in 96-dpi units.
    fn measure(&self, f: F, s: &str) -> f32 {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut size = windows::Win32::Foundation::SIZE::default();
        // SAFETY: our DC and font.
        unsafe {
            // Deselected again so a font set can be deleted (DPI change) while the DC lives.
            let old = SelectObject(self.dc, HGDIOBJ(self.fonts.get(f).0));
            let _ = GetTextExtentPoint32W(self.dc, &wide, &mut size);
            SelectObject(self.dc, old);
        }
        size.cx as f32 / self.s
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|h| h.0.contains(x, y))
            .map(|h| h.1)
    }

    fn hit(&mut self, p: &Painter, a: R, h: Hit) {
        let a = match p.clip {
            Some(c) => a.clip(c),
            None => a,
        };
        if a.w > 0.0 && a.h > 0.0 {
            self.hits.push((a, h));
        }
    }

    /// Puts an EDIT here this frame (or hides it when `a` is None).
    fn place(&mut self, h: HWND, a: Option<R>) {
        let s = self.s;
        let rc = a.map(|a| RECT {
            left: (a.x * s).round() as i32,
            top: (a.y * s).round() as i32,
            right: ((a.x + a.w) * s).round() as i32,
            bottom: ((a.y + a.h) * s).round() as i32,
        });
        if let Some(w) = self.want.iter_mut().find(|w| w.0 == h) {
            w.1 = rc;
        }
    }

    fn group(&mut self) {
        let now = now_ms();
        self.items.clear();
        let mut y = 4.0;
        let mut last = String::new();
        for (i, row) in self.rows.iter().enumerate() {
            let day = day_label(row.created_ms, now);
            if day != last {
                self.items.push((y, DAY_H, Item::Day(day.clone())));
                y += DAY_H;
                last = day;
            }
            self.items.push((y, ROW_H, Item::Row(i)));
            y += ROW_H;
        }
        self.list_h = y + 4.0;
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self
            .scroll
            .clamp(0.0, (self.list_h - self.list_view.h).max(0.0));
        self.set_scroll = self
            .set_scroll
            .clamp(0.0, (self.set_h - self.set_view.h).max(0.0));
    }

    /// Scrolls the selected row into view, with its day header when that's just above.
    fn reveal(&mut self) {
        let Some(sel) = self.sel else { return };
        let Some(k) = self
            .items
            .iter()
            .position(|it| matches!(it.2, Item::Row(i) if i == sel))
        else {
            return;
        };
        let (mut y, h, _) = self.items[k];
        if k > 0 && matches!(self.items[k - 1].2, Item::Day(_)) {
            y -= DAY_H;
        }
        let bottom = self.items[k].0 + h;
        if y < self.scroll {
            self.scroll = y;
        } else if bottom > self.scroll + self.list_view.h {
            self.scroll = bottom - self.list_view.h;
        }
        self.clamp_scroll();
    }

    fn detail_text(&self) -> String {
        match self.sel.and_then(|i| self.rows.get(i)) {
            Some(r) => r.text.replace("\r\n", "\n").replace('\n', "\r\n"),
            None => String::new(),
        }
    }

    fn paint(&mut self, hdc: HDC) {
        let mut rc = RECT::default();
        // SAFETY: plain query.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rc) };
        let (w, h) = (rc.right.max(1), rc.bottom.max(1));
        if self.canvas.as_ref().is_none_or(|c| c.w != w || c.h != h) {
            self.canvas = None;
            self.canvas = Canvas::new(self.dc, w, h);
        }
        let Some(canvas) = self.canvas.take() else {
            return;
        };
        // SAFETY: the canvas is w × h pixels and alive for this function.
        unsafe {
            std::slice::from_raw_parts_mut(canvas.bits, (w * h) as usize).fill(BG.px());
        }
        let mut p = Painter {
            s: self.s,
            px: canvas.bits,
            w,
            h,
            clip: None,
            texts: Vec::new(),
        };
        self.frame(Some(&mut p));
        // SAFETY: drawing into our memory DC, then copying it to the window's.
        unsafe {
            let old = SelectObject(self.dc, HGDIOBJ(canvas.bmp.0));
            let _ = GdiFlush();
            for t in &mut p.texts {
                if let Some(c) = t.clip {
                    IntersectClipRect(self.dc, c.left, c.top, c.right, c.bottom);
                }
                SelectObject(self.dc, HGDIOBJ(t.font.0));
                SetTextColor(self.dc, t.color.cr());
                let mut rect = t.rect;
                DrawTextW(self.dc, &mut t.text, &mut rect, t.flags);
                if t.clip.is_some() {
                    SelectClipRgn(self.dc, None);
                }
            }
            let _ = BitBlt(hdc, 0, 0, w, h, Some(self.dc), 0, 0, SRCCOPY);
            SelectObject(self.dc, old);
        }
        self.canvas = Some(canvas);
    }

    /// Lays out the whole window and, with a painter holding pixels, paints it.
    fn frame(&mut self, p: Option<&mut Painter>) {
        let mut scratch = Painter {
            s: self.s,
            px: std::ptr::null_mut(),
            w: 0,
            h: 0,
            clip: None,
            texts: Vec::new(),
        };
        let p = p.unwrap_or(&mut scratch);
        self.hits.clear();
        self.want = self.edits.all().iter().map(|&(_, h)| (h, None)).collect();
        let (w, h) = self.client();
        self.sidebar(p, h);
        match self.page {
            Page::History => self.history(p, w, h),
            Page::Dictionary => self.dictionary(p, w, h),
            Page::Settings => self.settings_page(p, w, h),
        }
    }

    fn sidebar(&mut self, p: &mut Painter, h: f32) {
        p.fill(r(SIDE, 0.0, 1.0 / self.s, h), 0.0, LINE);
        p.fill(r(24.0, 25.0, 10.0, 10.0), 5.0, RED);
        p.text(
            "dictap",
            self.fonts.get(F::Label),
            INK,
            r(42.0, 20.0, 150.0, 20.0),
            LINE1,
        );

        let mut y = 64.0;
        for (page, glyph, name) in [
            (Page::History, G_HISTORY, "History"),
            (Page::Dictionary, G_BOOK, "Dictionary"),
            (Page::Settings, G_SETTINGS, "Settings"),
        ] {
            let item = r(12.0, y, SIDE - 24.0, 36.0);
            let on = self.page == page;
            let hover = self.hover == Some(Hit::Nav(page));
            if on {
                p.fill(item, 8.0, SELECT);
            } else if hover {
                p.fill(item, 8.0, HOVER);
            }
            let ink = if on || hover { INK } else { INK2 };
            p.text(
                &glyph.to_string(),
                self.fonts.get(F::Icon),
                ink,
                r(item.x + 12.0, item.y, 20.0, item.h),
                LINE1,
            );
            p.text(
                name,
                self.fonts.get(F::Body),
                ink,
                r(item.x + 42.0, item.y, 140.0, item.h),
                LINE1,
            );
            self.hit(p, item, Hit::Nav(page));
            y += 40.0;
        }

        // The shortcut, as keycaps.
        let card = r(12.0, h - 16.0 - 74.0, SIDE - 24.0, 74.0);
        p.fill(card, 10.0, CARD);
        let mut x = card.x + 14.0;
        let hotkey = self.settings.hotkey.clone();
        for part in hotkey.split('+') {
            let kw = self.measure(F::Small, part) + 16.0;
            let cap = r(x, card.y + 14.0, kw, 24.0);
            p.fill(cap, 6.0, FIELD);
            p.shape(cap, 6.0, LINE_HI, 1.0, Some(1.0));
            p.fill(
                r(cap.x + 1.0, cap.bottom() - 2.0, cap.w - 2.0, 1.0),
                0.0,
                LINE,
            );
            p.text(part, self.fonts.get(F::Small), INK, cap, LINE1 | DT_CENTER);
            x += kw + 6.0;
        }
        p.text(
            "to start or stop dictating",
            self.fonts.get(F::Small),
            INK3,
            r(card.x + 14.0, card.y + 44.0, card.w - 28.0, 18.0),
            LINE1,
        );
    }

    fn header(&mut self, p: &mut Painter, title: &str, sub: &str) {
        let x = SIDE + PAD;
        p.text(
            title,
            self.fonts.get(F::Title),
            INK,
            r(x, 22.0, 400.0, 36.0),
            LINE1,
        );
        p.text(
            self.action_note.as_deref().unwrap_or(sub),
            self.fonts.get(F::Small),
            INK3,
            r(x, 60.0, 600.0, 18.0),
            LINE1,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        p: &mut Painter,
        right: f32,
        y: f32,
        label: &str,
        glyph: Option<char>,
        kind: Kind,
        hit: Hit,
    ) -> R {
        let gw = if glyph.is_some() { 22.0 } else { 0.0 };
        let bw = self.measure(F::Label, label) + gw + 28.0;
        let b = r(right - bw, y, bw, 34.0);
        let hover = self.hover == Some(hit);
        let down = hover && self.pressed == Some(hit);
        let (fill, ink) = match kind {
            Kind::Primary => (
                Some(if down {
                    Rgb(0xD8, 0xD8, 0xDE)
                } else if hover {
                    Rgb(0xFF, 0xFF, 0xFF)
                } else {
                    INK
                }),
                Rgb(0x14, 0x14, 0x16),
            ),
            Kind::Secondary => (Some(if hover { SELECT } else { FIELD }), INK),
            Kind::Ghost => (hover.then_some(HOVER), if hover { INK } else { INK2 }),
            Kind::Danger => (
                Some(if hover { Rgb(0xFF, 0x5E, 0x55) } else { RED }),
                Rgb(0xFF, 0xFF, 0xFF),
            ),
        };
        if let Some(f) = fill {
            p.fill(b, 8.0, f);
        }
        if kind == Kind::Secondary {
            p.shape(b, 8.0, LINE_HI, 0.6, Some(1.0));
        }
        let mut tx = b.x + 14.0;
        if let Some(g) = glyph {
            p.text(
                &g.to_string(),
                self.fonts.get(F::IconSmall),
                ink,
                r(tx, b.y, 18.0, b.h),
                LINE1,
            );
            tx += gw;
        }
        p.text(
            label,
            self.fonts.get(F::Label),
            ink,
            r(tx, b.y, b.right() - tx, b.h),
            LINE1,
        );
        self.hit(p, b, hit);
        b
    }

    /// A text field frame with its EDIT placed inside.
    fn field(&mut self, p: &mut Painter, f: R, id: usize, glyph: Option<char>, view: Option<R>) {
        let Some(h) = self.edits.by_id(id) else {
            return;
        };
        // SAFETY: plain focus query.
        let focused = unsafe { GetFocus() } == h;
        p.fill(f, 8.0, FIELD);
        let edge = if focused {
            ACCENT
        } else if self.hover == Some(Hit::Field(id)) {
            LINE_HI
        } else {
            LINE
        };
        p.shape(f, 8.0, edge, 1.0, Some(if focused { 1.5 } else { 1.0 }));
        let mut x = f.x + 12.0;
        if let Some(g) = glyph {
            p.text(
                &g.to_string(),
                self.fonts.get(F::IconSmall),
                INK3,
                r(x, f.y, 16.0, f.h),
                LINE1,
            );
            x += 24.0;
        }
        self.hit(p, f, Hit::Field(id));
        let eh = self.fonts.body_h as f32 / self.s;
        let at = r(x, f.y + (f.h - eh) / 2.0, f.right() - 10.0 - x, eh);
        let visible = view.is_none_or(|v| f.within(v));
        self.place(h, visible.then_some(at));
    }

    fn history(&mut self, p: &mut Painter, w: f32, h: f32) {
        let sub = if self.total == 0 {
            "Everything you dictate lands here".to_string()
        } else if self.total == 1 {
            "1 dictation".to_string()
        } else {
            format!("{} dictations", thousands(self.total))
        };
        self.header(p, "History", &sub);
        let x0 = SIDE + PAD;
        let right = w - PAD;

        let clear = self.button(
            p,
            right,
            28.0,
            "Clear…",
            Some(G_DELETE),
            Kind::Ghost,
            Hit::ClearMenu,
        );
        let search = r(clear.x - 10.0 - 280.0, 28.0, 280.0, 34.0);
        self.field(p, search, ID_SEARCH, Some(G_SEARCH), None);

        let mut top = BODY_TOP;
        if let Some(c) = &self.clear {
            let (count, what, cutoff) = (c.count, c.what, c.cutoff);
            let banner = r(x0, top, right - x0, 54.0);
            p.fill(banner, 10.0, CARD);
            p.shape(banner, 10.0, AMBER, 0.45, Some(1.0));
            let msg = match (count, what.is_empty()) {
                (0, false) => format!("No dictations {what}."),
                (0, true) => "History is already empty.".into(),
                (n, _) => {
                    let n = if n == 1 {
                        "1 dictation".into()
                    } else {
                        format!("{} dictations", thousands(n))
                    };
                    if cutoff == i64::MAX {
                        format!("Delete all {n}? This can't be undone.")
                    } else {
                        format!("Delete {n} {what}? This can't be undone.")
                    }
                }
            };
            let bx = banner.right() - 10.0;
            let by = banner.y + 10.0;
            let text_right = if count > 0 {
                let yes = self.button(p, bx, by, "Delete", None, Kind::Danger, Hit::ClearYes);
                let no = self.button(
                    p,
                    yes.x - 8.0,
                    by,
                    "Cancel",
                    None,
                    Kind::Secondary,
                    Hit::ClearNo,
                );
                no.x
            } else {
                self.button(p, bx, by, "OK", None, Kind::Secondary, Hit::ClearNo)
                    .x
            };
            p.text(
                &msg,
                self.fonts.get(F::Body),
                INK,
                r(
                    banner.x + 18.0,
                    banner.y,
                    text_right - banner.x - 26.0,
                    banner.h,
                ),
                LINE1,
            );
            top += 54.0 + 14.0;
        }

        let bottom = h - PAD + 4.0;
        let list_w = ((right - x0) * 0.42).clamp(300.0, 440.0);
        let list = r(x0, top, list_w, bottom - top);
        let detail = r(
            list.right() + 16.0,
            top,
            right - list.right() - 16.0,
            bottom - top,
        );
        self.list(p, list);
        self.detail(p, detail);
    }

    fn list(&mut self, p: &mut Painter, card: R) {
        p.fill(card, 12.0, CARD);
        let view = card.inset(6.0, 6.0);
        self.list_view = view;
        self.clamp_scroll();
        self.hit(p, view, Hit::List);
        if self.rows.is_empty() {
            let (a, b) = if self.total == 0 {
                (
                    "No dictations yet".to_string(),
                    format!(
                        "Press {} anywhere to start",
                        self.settings.hotkey.replace('+', " + ")
                    ),
                )
            } else {
                (
                    "No matches".to_string(),
                    "Try a different search".to_string(),
                )
            };
            let mid = card.y + card.h / 2.0;
            p.text(
                &a,
                self.fonts.get(F::Label),
                INK2,
                r(card.x, mid - 22.0, card.w, 20.0),
                LINE1 | DT_CENTER,
            );
            p.text(
                &b,
                self.fonts.get(F::Small),
                INK3,
                r(card.x, mid, card.w, 18.0),
                LINE1 | DT_CENTER,
            );
            return;
        }
        let overflow = self.list_h > view.h;
        let row_w = view.w - if overflow { 8.0 } else { 0.0 };
        p.clip = Some(view);
        for k in 0..self.items.len() {
            let (iy, ih) = (self.items[k].0, self.items[k].1);
            let y = view.y + iy - self.scroll;
            if y + ih < view.y {
                continue;
            }
            if y > view.bottom() {
                break;
            }
            match &self.items[k].2 {
                Item::Day(label) => {
                    let label = label.clone();
                    p.text(
                        &label,
                        self.fonts.get(F::Label),
                        INK3,
                        r(view.x + 12.0, y + 12.0, row_w - 24.0, 18.0),
                        LINE1,
                    );
                }
                &Item::Row(i) => {
                    let row = r(view.x, y, row_w, ih - 2.0);
                    let on = self.sel == Some(i);
                    if on {
                        p.fill(row, 8.0, SELECT);
                    } else if self.hover == Some(Hit::Row(i)) {
                        p.fill(row, 8.0, HOVER);
                    }
                    let rw = &self.rows[i];
                    let (created, dur) = (rw.created_ms, rw.duration_ms);
                    let tag = match rw.status.as_str() {
                        store::FAILED => Some("Failed"),
                        store::PROVISIONAL => Some("Incomplete"),
                        _ => None,
                    };
                    let text = preview(&rw.text);
                    let meta = r(row.x + 14.0, row.y + 10.0, row.w - 28.0, 16.0);
                    let time = clock(created);
                    p.text(&time, self.fonts.get(F::Small), INK3, meta, LINE1);
                    if let Some(tag) = tag {
                        let tx = meta.x + self.measure(F::Small, &time) + 8.0;
                        p.text(
                            tag,
                            self.fonts.get(F::Small),
                            AMBER,
                            r(tx, meta.y, 120.0, meta.h),
                            LINE1,
                        );
                    }
                    if let Some(d) = dur {
                        p.text(
                            &duration(d),
                            self.fonts.get(F::Small),
                            INK3,
                            meta,
                            LINE1 | DT_RIGHT,
                        );
                    }
                    let (body, ink) = if text.is_empty() {
                        ("No text".to_string(), INK3)
                    } else {
                        (text, if on { INK } else { Rgb(0xE4, 0xE4, 0xE8) })
                    };
                    p.text(
                        &body,
                        self.fonts.get(F::Body),
                        ink,
                        r(row.x + 14.0, row.y + 30.0, row.w - 28.0, 42.0),
                        DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS,
                    );
                    self.hit(p, row, Hit::Row(i));
                }
            }
        }
        p.clip = None;
        if overflow {
            let th = (view.h * view.h / self.list_h).max(36.0);
            let ty = view.y + self.scroll / (self.list_h - view.h) * (view.h - th);
            let active = self.drag.is_some() || self.hover == Some(Hit::Thumb);
            p.shape(
                r(card.right() - 9.0, ty, 4.0, th),
                2.0,
                INK,
                if active { 0.4 } else { 0.16 },
                None,
            );
            self.hit(p, r(card.right() - 14.0, ty, 14.0, th), Hit::Thumb);
        }
    }

    fn detail(&mut self, p: &mut Painter, card: R) {
        p.fill(card, 12.0, CARD);
        let Some(i) = self.sel.filter(|&i| i < self.rows.len()) else {
            let mid = card.y + card.h / 2.0;
            let msg = if self.rows.is_empty() {
                ""
            } else {
                "Select a dictation to read it here"
            };
            p.text(
                msg,
                self.fonts.get(F::Small),
                INK3,
                r(card.x, mid - 9.0, card.w, 18.0),
                LINE1 | DT_CENTER,
            );
            return;
        };
        let row = &self.rows[i];
        let x = card.x + 22.0;
        let inner_w = card.w - 44.0;
        let day = day_label(row.created_ms, now_ms());
        let title = format!("{day} · {}", clock(row.created_ms));
        let mut meta = Vec::new();
        if let Some(d) = row.duration_ms {
            meta.push(format!("{:.1} s", d as f64 / 1000.0));
        }
        let words = row.text.split_whitespace().count();
        meta.push(if words == 1 {
            "1 word".into()
        } else {
            format!("{} words", thousands(words as i64))
        });
        if let Some(m) = &row.model {
            meta.push(m.clone());
        }
        let error = row.error.clone();
        let (has_text, has_audio) = (!row.text.is_empty(), row.audio_path.is_some());
        p.text(
            &title,
            self.fonts.get(F::Label),
            INK,
            r(x, card.y + 18.0, inner_w, 20.0),
            LINE1,
        );
        p.text(
            &meta.join("  ·  "),
            self.fonts.get(F::Small),
            INK3,
            r(x, card.y + 40.0, inner_w, 18.0),
            LINE1,
        );
        let mut text_top = card.y + 72.0;
        if let Some(e) = error {
            p.text(
                &e,
                self.fonts.get(F::Small),
                AMBER,
                r(x, card.y + 60.0, inner_w, 18.0),
                LINE1,
            );
            text_top += 18.0;
        }
        p.fill(r(x, text_top - 10.0, inner_w, 1.0 / self.s), 0.0, LINE);

        let by = card.bottom() - 18.0 - 34.0;
        p.fill(r(x, by - 14.0, inner_w, 1.0 / self.s), 0.0, LINE);
        let (label, glyph) = if self.copied {
            ("Copied", G_CHECK)
        } else {
            ("Copy", G_COPY)
        };
        let mut left = x;
        if has_text {
            let bw = self
                .measure(F::Label, label)
                .max(self.measure(F::Label, "Copied"))
                + 22.0
                + 28.0;
            let b = self.button(
                p,
                left + bw,
                by,
                label,
                Some(glyph),
                Kind::Primary,
                Hit::Copy,
            );
            left = b.right() + 8.0;
        }
        if has_audio {
            let bw = self.measure(F::Label, "Retry") + 22.0 + 28.0;
            self.button(
                p,
                left + bw,
                by,
                "Retry",
                Some(G_RETRY),
                Kind::Secondary,
                Hit::Retry,
            );
        }
        if self.armed {
            self.button(
                p,
                card.right() - 18.0,
                by,
                "Click again to delete",
                Some(G_DELETE),
                Kind::Danger,
                Hit::Delete,
            );
        } else {
            self.button(
                p,
                card.right() - 18.0,
                by,
                "Delete",
                Some(G_DELETE),
                Kind::Ghost,
                Hit::Delete,
            );
        }
        let edit = r(x - 2.0, text_top, inner_w + 10.0, by - 24.0 - text_top);
        self.place(self.edits.detail, (edit.h > 20.0).then_some(edit));
    }

    fn dictionary(&mut self, p: &mut Painter, w: f32, h: f32) {
        let n = self.words.len();
        let sub = format!(
            "Names and words Gemini should spell your way, one per line · {} saved",
            if n == 1 {
                "1 word".into()
            } else {
                format!("{} words", thousands(n as i64))
            }
        );
        self.header(p, "Dictionary", &sub);
        let card = r(
            SIDE + PAD,
            BODY_TOP,
            w - SIDE - 2.0 * PAD,
            h - BODY_TOP - PAD + 4.0,
        );
        p.fill(card, 12.0, CARD);
        self.place(self.edits.words, Some(card.inset(18.0, 14.0)));
    }

    fn settings_page(&mut self, p: &mut Painter, w: f32, h: f32) {
        self.header(p, "Settings", "Changes are saved as you make them");
        let x = SIDE + PAD;
        let cw = (w - SIDE - 2.0 * PAD).min(760.0);
        let view = r(
            SIDE + 1.0,
            BODY_TOP - 8.0,
            w - SIDE - 1.0,
            h - BODY_TOP + 8.0,
        );
        self.set_view = view;
        p.clip = Some(view);
        let mut y = BODY_TOP - self.set_scroll;

        let hotkey_desc = if self.hotkey_error {
            (
                "Not a shortcut dictap understands — try Ctrl+Win or Ctrl+Alt+Space",
                RED,
            )
        } else {
            ("Press once to start, again to stop and paste", INK3)
        };
        let key_note = self.key_note.clone();
        let key_desc = match &key_note {
            Some((n, good)) => (n.as_str(), if *good { GREEN } else { RED }),
            None => ("Stored in Windows Credential Manager, never on disk", INK3),
        };
        let keep = self.settings.keep_days;
        let (sounds, autostart) = (self.settings.sounds, self.autostart);

        // Dictation
        let card = self.section(p, x, &mut y, cw, "Dictation", 3);
        self.row_text(p, card, 0, "Shortcut", hotkey_desc);
        self.field(
            p,
            r(card.right() - 18.0 - 200.0, card.y + 15.0, 200.0, 34.0),
            ID_HOTKEY,
            None,
            Some(view),
        );
        self.row_text(
            p,
            card,
            1,
            "Language",
            ("A language code such as en-GB, or empty to detect it", INK3),
        );
        self.field(
            p,
            r(
                card.right() - 18.0 - 200.0,
                card.y + SET_ROW_H + 15.0,
                200.0,
                34.0,
            ),
            ID_LANGUAGE,
            None,
            Some(view),
        );
        self.row_text(
            p,
            card,
            2,
            "Sounds",
            ("A soft chime when dictation starts and stops", INK3),
        );
        self.toggle(p, card, 2, sounds, Hit::Sounds);

        // History
        let card = self.section(p, x, &mut y, cw, "History", 1);
        self.row_text(
            p,
            card,
            0,
            "Keep dictations",
            ("Older ones are deleted automatically", INK3),
        );
        let opts: [(&str, u32); 4] = [
            ("Forever", 0),
            ("90 days", 90),
            ("30 days", 30),
            ("7 days", 7),
        ];
        let widths: Vec<f32> = opts
            .iter()
            .map(|o| self.measure(F::Body, o.0) + 24.0)
            .collect();
        let total: f32 = widths.iter().sum::<f32>() + 6.0;
        let seg = r(card.right() - 18.0 - total, card.y + 15.0, total, 34.0);
        p.fill(seg, 8.0, FIELD);
        let mut sx = seg.x + 3.0;
        for ((label, days), sw) in opts.iter().zip(widths) {
            let o = r(sx, seg.y + 3.0, sw, seg.h - 6.0);
            let on = keep == *days;
            if on {
                p.fill(o, 6.0, SELECT);
                p.shape(o, 6.0, LINE_HI, 0.8, Some(1.0));
            }
            let ink = if on || self.hover == Some(Hit::Keep(*days)) {
                INK
            } else {
                INK2
            };
            p.text(label, self.fonts.get(F::Body), ink, o, LINE1 | DT_CENTER);
            self.hit(p, o, Hit::Keep(*days));
            sx += sw;
        }

        // General
        let card = self.section(p, x, &mut y, cw, "General", 1);
        self.row_text(
            p,
            card,
            0,
            "Start with Windows",
            ("Open dictap in the tray when you sign in", INK3),
        );
        self.toggle(p, card, 0, autostart, Hit::Autostart);

        // Gemini
        let card = self.section(p, x, &mut y, cw, "Gemini", 2);
        self.row_text(p, card, 0, "API key", key_desc);
        let save = self.button(
            p,
            card.right() - 18.0,
            card.y + 15.0,
            "Save",
            None,
            Kind::Secondary,
            Hit::SaveKey,
        );
        self.field(
            p,
            r(save.x - 8.0 - 220.0, card.y + 15.0, 220.0, 34.0),
            ID_KEY,
            None,
            Some(view),
        );
        self.row_text(
            p,
            card,
            1,
            "Test connection",
            (
                "Checks the saved key; the result shows above the taskbar",
                INK3,
            ),
        );
        self.button(
            p,
            card.right() - 18.0,
            card.y + SET_ROW_H + 15.0,
            "Test",
            None,
            Kind::Secondary,
            Hit::TestKey,
        );

        // Import
        let card = self.section(p, x, &mut y, cw, "Import", 1);
        self.row_text(
            p,
            card,
            0,
            "OpenWhispr",
            (
                "Copies your OpenWhispr history and dictionary; safe to repeat",
                INK3,
            ),
        );
        self.button(
            p,
            card.right() - 18.0,
            card.y + 15.0,
            "Import",
            None,
            Kind::Secondary,
            Hit::Import,
        );

        y += 24.0;
        p.clip = None;
        self.set_h = y + self.set_scroll - view.y;
        let max = (self.set_h - view.h).max(0.0);
        if max > 0.0 {
            let th = (view.h * view.h / self.set_h).max(36.0);
            let ty = view.y + self.set_scroll / max * (view.h - th);
            p.shape(r(w - 9.0, ty, 4.0, th), 2.0, INK, 0.16, None);
        }
    }

    /// A titled card of `n` rows; returns the card and advances `y` past it.
    fn section(
        &mut self,
        p: &mut Painter,
        x: f32,
        y: &mut f32,
        w: f32,
        title: &str,
        n: usize,
    ) -> R {
        p.text(
            title,
            self.fonts.get(F::Label),
            INK2,
            r(x + 2.0, *y, w, 18.0),
            LINE1,
        );
        *y += 26.0;
        let card = r(x, *y, w, SET_ROW_H * n as f32);
        p.fill(card, 12.0, CARD);
        for i in 1..n {
            p.fill(
                r(
                    card.x + 18.0,
                    card.y + SET_ROW_H * i as f32,
                    card.w - 36.0,
                    1.0 / self.s,
                ),
                0.0,
                LINE,
            );
        }
        *y += card.h + 26.0;
        card
    }

    fn row_text(
        &mut self,
        p: &mut Painter,
        card: R,
        i: usize,
        title: &str,
        (desc, ink): (&str, Rgb),
    ) {
        let y = card.y + SET_ROW_H * i as f32;
        let w = card.w * 0.55;
        p.text(
            title,
            self.fonts.get(F::Body),
            INK,
            r(card.x + 18.0, y + 12.0, w, 20.0),
            LINE1,
        );
        p.text(
            desc,
            self.fonts.get(F::Small),
            ink,
            r(card.x + 18.0, y + 33.0, w, 18.0),
            LINE1,
        );
    }

    fn toggle(&mut self, p: &mut Painter, card: R, i: usize, on: bool, hit: Hit) {
        let t = r(
            card.right() - 18.0 - 42.0,
            card.y + SET_ROW_H * i as f32 + 21.0,
            42.0,
            22.0,
        );
        let hover = self.hover == Some(hit);
        if on {
            p.fill(t, 11.0, if hover { Rgb(0x5A, 0x9C, 0xFF) } else { ACCENT });
        } else {
            p.fill(
                t,
                11.0,
                if hover {
                    Rgb(0x44, 0x44, 0x4B)
                } else {
                    Rgb(0x3A, 0x3A, 0x40)
                },
            );
        }
        let kx = if on { t.right() - 19.0 } else { t.x + 3.0 };
        p.fill(
            r(kx, t.y + 3.0, 16.0, 16.0),
            8.0,
            if on { Rgb(0xFF, 0xFF, 0xFF) } else { INK2 },
        );
        // Generous target: the whole control plus a margin.
        self.hit(p, t.inset(-6.0, -8.0), hit);
    }
}

fn scroll_by(dy: f32, settings: bool) {
    with(|a| {
        if settings {
            a.set_scroll += dy;
        } else {
            a.scroll += dy;
        }
        a.clamp_scroll();
    });
    refresh();
}

fn mouse(lparam: LPARAM) -> (f32, f32, f32) {
    let x = (lparam.0 & 0xFFFF) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
    let s = with(|a| a.s).unwrap_or(1.0);
    (x / s, y / s, s)
}

fn key_down(key: u16) {
    let Some((page, sel, n, view_h)) = with(|a| (a.page, a.sel, a.rows.len(), a.list_view.h))
    else {
        return;
    };
    if page != Page::History || n == 0 {
        return;
    }
    // SAFETY: plain key state query.
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let page_rows = ((view_h / ROW_H) as usize).max(1);
    let cur = sel.unwrap_or(0);
    let to = match key {
        k if k == VK_UP.0 => Some(if sel.is_none() {
            0
        } else {
            cur.saturating_sub(1)
        }),
        k if k == VK_DOWN.0 => Some(if sel.is_none() {
            0
        } else {
            (cur + 1).min(n - 1)
        }),
        k if k == VK_HOME.0 => Some(0),
        k if k == VK_END.0 => Some(n - 1),
        k if k == VK_PRIOR.0 => Some(cur.saturating_sub(page_rows)),
        k if k == VK_NEXT.0 => Some((cur + page_rows).min(n - 1)),
        k if k == VK_RETURN.0 => {
            copy();
            None
        }
        k if k == u16::from(b'C') && ctrl => {
            copy();
            None
        }
        k if k == VK_DELETE.0 => {
            delete();
            None
        }
        _ => None,
    };
    if let Some(i) = to {
        select(Some(i));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: standard paint bracket on our window.
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            with(|a| a.paint(hdc));
            // SAFETY: closes the bracket above.
            let _ = unsafe { EndPaint(hwnd, &ps) };
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_SIZE => refresh(),
        WM_GETMINMAXINFO => {
            let dpi = ui::dpi(hwnd);
            // SAFETY: WM_GETMINMAXINFO carries a MINMAXINFO.
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = ui::px(MIN_W, dpi);
            mmi.ptMinTrackSize.y = ui::px(MIN_H, dpi);
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xFFFF) as u32;
            let s = dpi as f32 / 96.0;
            let fonts = Fonts::new(s);
            let body = fonts.get(F::Body);
            let Some((edits, old)) = with(|a| {
                a.s = s;
                a.canvas = None;
                (a.edits, std::mem::replace(&mut a.fonts, fonts))
            }) else {
                return LRESULT(0);
            };
            for (_, h) in edits.all() {
                ui::send(h, WM_SETFONT, body.0 as usize, 1);
            }
            drop(old);
            // SAFETY: lparam is the suggested RECT; resizing our own window.
            unsafe {
                let rc = *(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            refresh();
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
            if let Some(pt) = PAINT.with(Cell::get) {
                let card =
                    HWND(lparam.0 as *mut _) == pt.detail || HWND(lparam.0 as *mut _) == pt.words;
                let hdc = HDC(wparam.0 as *mut _);
                // SAFETY: colouring the control's DC for this paint.
                unsafe {
                    SetTextColor(hdc, INK.cr());
                    SetBkColor(hdc, if card { CARD.cr() } else { FIELD.cr() });
                }
                return LRESULT(if card { pt.card.0 } else { pt.field.0 } as isize);
            }
            // SAFETY: default handling.
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            let code = (wparam.0 >> 16) as u32;
            match code {
                EN_CHANGE if id == ID_SEARCH => start_timer(SEARCH_TIMER, 150),
                EN_SETFOCUS => invalidate(),
                EN_KILLFOCUS => {
                    invalidate();
                    // SAFETY: our own window.
                    let _ =
                        unsafe { PostMessageW(Some(hwnd), WM_APP_COMMIT, WPARAM(id), LPARAM(0)) };
                }
                _ => {}
            }
        }
        WM_APP_COMMIT => commit(wparam.0),
        WM_APP_CHANGED => reload(false),
        WM_APP_RESULT => drain_results(),
        WM_TIMER => {
            // SAFETY: our own timer.
            let _ = unsafe { KillTimer(Some(hwnd), wparam.0) };
            match wparam.0 {
                RESULT_TIMER => {
                    drain_results();
                    if with(|a| !a.pending.is_empty()) == Some(true) {
                        start_timer(RESULT_TIMER, 200);
                    }
                }
                SEARCH_TIMER => reload(true),
                COPIED_TIMER => {
                    with(|a| a.copied = false);
                    refresh();
                }
                ARM_TIMER => {
                    with(|a| a.armed = false);
                    refresh();
                }
                NOTE_TIMER => {
                    with(|a| {
                        a.key_note = None;
                        a.action_note = None;
                    });
                    refresh();
                }
                _ => {}
            }
        }
        WM_SETFOCUS | WM_KILLFOCUS => invalidate(),
        WM_MOUSEMOVE => {
            let (x, y, _) = mouse(lparam);
            let redraw = with(|a| {
                if !a.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: tme is valid.
                    let _ = unsafe { TrackMouseEvent(&mut tme) };
                    a.tracking = true;
                }
                if let Some((y0, s0, settings)) = a.drag {
                    let (view, total, cur) = if settings {
                        (a.set_view, a.set_h, &mut a.set_scroll)
                    } else {
                        (a.list_view, a.list_h, &mut a.scroll)
                    };
                    let th = (view.h * view.h / total).max(36.0);
                    let range = (total - view.h).max(1.0);
                    *cur = s0 + (y - y0) * range / (view.h - th).max(1.0);
                    a.clamp_scroll();
                    return true;
                }
                let hover = a.hit_at(x, y);
                let changed = hover != a.hover;
                a.hover = hover;
                changed
            });
            if redraw == Some(true) {
                refresh();
            }
        }
        WM_MOUSELEAVE => {
            with(|a| {
                a.tracking = false;
                a.hover = None;
            });
            invalidate();
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y, _) = mouse(lparam);
            let Some((hit, edits, scroll, settings)) =
                with(|a| (a.hit_at(x, y), a.edits, a.scroll, a.page == Page::Settings))
            else {
                return LRESULT(0);
            };
            with(|a| a.pressed = hit);
            match hit {
                Some(Hit::Field(id)) => {
                    if let Some(h) = edits.by_id(id) {
                        // SAFETY: focusing our own child.
                        let _ = unsafe { SetFocus(Some(h)) };
                    }
                }
                _ => {
                    // SAFETY: focusing our own window (commits any field being edited).
                    let _ = unsafe { SetFocus(Some(hwnd)) };
                }
            }
            match hit {
                Some(Hit::Row(i)) => {
                    select(Some(i));
                    if msg == WM_LBUTTONDBLCLK {
                        copy();
                    }
                }
                Some(Hit::Thumb) => {
                    with(|a| a.drag = Some((y, scroll, settings)));
                    // SAFETY: capturing to our own window for the drag.
                    unsafe { SetCapture(hwnd) };
                }
                _ => {}
            }
            invalidate();
        }
        WM_LBUTTONUP => {
            let (x, y, _) = mouse(lparam);
            let Some((pressed, now, dragging)) = with(|a| {
                let pressed = a.pressed.take();
                let dragging = a.drag.take().is_some();
                (pressed, a.hit_at(x, y), dragging)
            }) else {
                return LRESULT(0);
            };
            if dragging {
                // SAFETY: ends our capture.
                let _ = unsafe { ReleaseCapture() };
                refresh();
            } else if let Some(h) = pressed.filter(|&h| Some(h) == now) {
                click(h);
            } else {
                invalidate();
            }
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            // SAFETY: plain conversion.
            let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
            if let Some((page, view, s)) = with(|a| (a.page, a.list_view, a.s)) {
                let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
                let dy = -delta / 120.0 * 96.0;
                match page {
                    Page::History if view.contains(x, y) => scroll_by(dy, false),
                    Page::Settings => scroll_by(dy, true),
                    _ => {}
                }
            }
        }
        WM_KEYDOWN => key_down(wparam.0 as u16),
        WM_CHAR => {
            // Typing in the list starts a search.
            let c = wparam.0 as u32;
            if let Some((Page::History, search)) = with(|a| (a.page, a.edits.search))
                && c >= 0x20
                && c != 0x7F
            {
                // SAFETY: focusing our own child and forwarding the character.
                unsafe {
                    let _ = SetFocus(Some(search));
                    SendMessageW(search, WM_CHAR, Some(wparam), Some(lparam));
                }
            }
        }
        WM_CLOSE => {
            commit(ID_WORDS);
            commit(ID_HOTKEY);
            commit(ID_LANGUAGE);
            // SAFETY: destroying our own window.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => {
            HWND_.store(0, Ordering::Release);
            RESULTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            if let Some(pt) = PAINT.with(Cell::take) {
                // SAFETY: the controls using them are gone.
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(pt.field.0));
                    let _ = DeleteObject(HGDIOBJ(pt.card.0));
                }
            }
            if let Some(mut a) = APP.with(|a| a.borrow_mut().take()) {
                a.canvas = None;
                // SAFETY: releasing what create() made.
                unsafe {
                    let _ = DeleteDC(a.dc);
                    for i in a.icons {
                        let _ = DestroyIcon(i);
                    }
                }
            }
        }
        // SAFETY: default handling.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

#[cfg(test)]
mod request_tests {
    use super::*;
    fn queue() -> Replies {
        Replies {
            reserved: Vec::new(),
            queued: VecDeque::new(),
        }
    }
    fn reply_for(id: RequestId, generation: u64) -> ActionResult {
        ActionResult {
            id,
            window_generation: generation,
            result: Ok(ActionSuccess::Copied),
        }
    }
    #[test]
    fn replies_reserve_capacity_and_reject_duplicates_or_stale_windows() {
        let mut q = queue();
        let ids: Vec<_> = (0..MAX_PENDING).map(|_| RequestId::next()).collect();
        for &id in &ids {
            assert!(q.reserve(id, 1));
        }
        assert!(!q.reserve(RequestId::next(), 1));
        for &id in &ids {
            assert!(q.enqueue(reply_for(id, 1)));
        }
        assert!(!q.enqueue(reply_for(ids[0], 1)));
        assert_eq!(q.queued.len(), MAX_PENDING);
        q.clear();
        let newer = RequestId::next();
        assert!(q.reserve(newer, 2));
        assert!(!q.enqueue(reply_for(ids[0], 1)));
        assert!(q.enqueue(reply_for(newer, 2)));
        q.queued.clear();
        q.release(newer);
        assert!(q.reserved.is_empty());
    }
    #[test]
    fn different_copy_rows_still_share_one_pending_action_kind() {
        assert!(same_kind(Pending::Copy(1), Pending::Copy(2)));
        assert!(same_kind(Pending::Settings, Pending::Settings));
        assert!(!same_kind(Pending::Key, Pending::Settings));
    }
}
