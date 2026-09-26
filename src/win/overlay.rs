//! The floating HUD near the bottom of the screen. A capsule shows the state (a pulsing
//! dot, a voice-reactive waveform and a timer while recording; animated dots while
//! busy; a check on success; an icon and message otherwise). Above it, a panel shows the
//! live transcript: words fade and rise in, interim words brighten when final, older
//! lines scroll up under a soft fade.
//!
//! Layered with per-pixel alpha, topmost, click-through, never takes focus. It runs on its
//! own thread. A frame timer runs only while something moves; when hidden there are no
//! timers and the drawing surface is freed.
//!
//! Rendering: GDI draws text as a grey coverage mask (grey = opacity) into one DIB; the
//! shapes (shadow, gradient fill, hairlines) are rasterised from signed distance fields
//! into a buffer that is cached until their geometry changes; each frame copies the cache,
//! blends in the text and glyphs, and hands the premultiplied BGRA to
//! `UpdateLayeredWindow`.

use super::ui;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLACKNESS, BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, CreateFontW, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, FW_NORMAL, FW_SEMIBOLD, GdiFlush, GetMonitorInfoW,
    GetTextExtentPoint32W, GetTextMetricsW, HBITMAP, HDC, HFONT, HGDIOBJ, IntersectClipRect,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow, PatBlt,
    SelectClipRgn, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMessageW, HWND_TOPMOST, KillTimer, MA_NOACTIVATE, MSG, PostMessageW, RegisterClassW,
    SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetTimer, SetWindowPos,
    ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_APP, WM_MOUSEACTIVATE, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{HSTRING, w};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Recording,
    Busy,
    Info,
    Error,
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

// Always dark: it floats over whatever app is underneath.
const FILL_TOP: Rgb = Rgb(0x2B, 0x2B, 0x30);
const FILL_BOTTOM: Rgb = Rgb(0x19, 0x19, 0x1C);
const FILL_ALPHA: f32 = 0.96;
const INK: Rgb = Rgb(0xF5, 0xF5, 0xF7);
const RED: Rgb = Rgb(0xFF, 0x45, 0x3A);
const GREEN: Rgb = Rgb(0x30, 0xD1, 0x58);
const AMBER: Rgb = Rgb(0xFF, 0xB3, 0x40);
const BLUE: Rgb = Rgb(0x64, 0xA8, 0xFF);

// Layout in 96-dpi px.
const MARGIN: i32 = 28; // room for shadows around the shapes
const BOTTOM_GAP: i32 = 28;
const RISE: f32 = 10.0; // slides up this far as it fades in
const CAP_H: i32 = 36;
const CAP_PAD: i32 = 16;
const PANEL_W: i32 = 540;
const PANEL_GAP: i32 = 10;
const PANEL_PAD_X: i32 = 20;
const PANEL_PAD_Y: i32 = 14;
const PANEL_RADIUS: f32 = 16.0;
const LINE_H: i32 = 24;
const MAX_LINES: i32 = 3;
const LABEL_PX: i32 = 13;
const TEXT_PX: i32 = 16;
const ICON: i32 = 16;
const ICON_GAP: i32 = 8;
const DOT: i32 = 8;
const PART_GAP: i32 = 10;
const BARS: usize = 7;
const BAR_W: f32 = 3.0;
const BAR_GAP: f32 = 3.0;
const BAR_MAX: f32 = 18.0;
const BUSY_DOTS_W: i32 = 23;

/// Interim words, not yet final.
const INTERIM: f32 = 0.5;
const WORD_IN: Duration = Duration::from_millis(220);
const CONTENT_IN: Duration = Duration::from_millis(180);
const CHECK_DRAW: Duration = Duration::from_millis(260);
const DONE_HOLD: Duration = Duration::from_millis(900);
/// The recording limit's countdown shows for this long before the cut-off.
const COUNTDOWN: Duration = Duration::from_secs(10);
/// Below this many seconds left the countdown turns red.
const COUNTDOWN_RED: u64 = 3;
const PILL_H: f32 = 20.0;
const PILL_PAD: f32 = 7.0;

const WM_APP_UPDATE: u32 = WM_APP + 10;
const HIDE_TIMER: usize = 1;
const FRAME_TIMER: usize = 2;
/// Transitions in flight.
const FRAME_FAST: u32 = 16;
/// Steady recording or busy: waveform, pulse and dots only.
const FRAME_SLOW: u32 = 33;

enum Cmd {
    Show {
        text: String,
        tone: Tone,
        hide_after: Option<Duration>,
        clear: bool,
    },
    Words(String, String),
    /// When recording will be cut off.
    Limit(Instant),
    Done,
    Hide,
}

static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
static CMDS: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static LEVEL: AtomicU32 = AtomicU32::new(0);
static STARTED: OnceLock<()> = OnceLock::new();

/// A status in the capsule; clears any transcript. Hides after `hide_after` if given.
pub fn show(text: &str, tone: Tone, hide_after: Option<Duration>) {
    send(Cmd::Show {
        text: text.to_string(),
        tone,
        hide_after,
        clear: true,
    });
}

/// Changes the capsule but keeps the transcript on screen (e.g. "Transcribing…").
pub fn status(text: &str, tone: Tone, hide_after: Option<Duration>) {
    send(Cmd::Show {
        text: text.to_string(),
        tone,
        hide_after,
        clear: false,
    });
}

/// The live transcript so far: settled text and the interim tail.
pub fn words(finals: &str, interim: &str) {
    send(Cmd::Words(finals.to_string(), interim.to_string()));
}

/// Recording stops `after` from now; the capsule counts down the last seconds.
pub fn limit(after: Duration) {
    send(Cmd::Limit(Instant::now() + after));
}

/// The text landed: a check, then fade out.
pub fn done() {
    send(Cmd::Done);
}

pub fn hide() {
    send(Cmd::Hide);
}

/// Microphone RMS (0..1) of the latest audio; drives the waveform. Cheap: no message.
pub fn level(rms: f32) {
    LEVEL.store(rms.to_bits(), Ordering::Relaxed);
}

fn send(cmd: Cmd) {
    STARTED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || run(tx))
            .expect("spawn overlay thread");
        let _ = rx.recv();
    });
    CMDS.lock().unwrap_or_else(|e| e.into_inner()).push(cmd);
    let hwnd = HWND_VAL.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: posting to our overlay window.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_APP_UPDATE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

thread_local! {
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

fn run(ready: std::sync::mpsc::Sender<()>) {
    // SAFETY: standard class registration, window creation and message loop on this thread.
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            let _ = ready.send(());
            return;
        };
        let class = w!("dictap.overlay");
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
        ) {
            Ok(hwnd) => {
                HUD.with(|h| *h.borrow_mut() = Some(Hud::new(hwnd)));
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

fn with_hud(f: impl FnOnce(&mut Hud)) {
    HUD.with(|h| {
        // try_borrow: a message sent to us mid-frame must not panic.
        if let Ok(mut h) = h.try_borrow_mut()
            && let Some(hud) = h.as_mut()
        {
            f(hud);
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_UPDATE => with_hud(|hud| {
            let cmds = std::mem::take(&mut *CMDS.lock().unwrap_or_else(|e| e.into_inner()));
            for cmd in cmds {
                hud.apply(cmd);
            }
            hud.frame();
        }),
        WM_TIMER if wparam.0 == FRAME_TIMER => with_hud(Hud::frame),
        WM_TIMER if wparam.0 == HIDE_TIMER => with_hud(|hud| {
            hud.set_hide(None);
            hud.shown = false;
            hud.frame();
        }),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

/// Eases towards a target, frame-rate independent.
#[derive(Clone, Copy)]
struct Spring {
    v: f32,
    t: f32,
}

impl Spring {
    const fn at(v: f32) -> Spring {
        Spring { v, t: v }
    }

    /// Returns whether it is still moving.
    fn step(&mut self, dt: f32, rate: f32, eps: f32) -> bool {
        let d = self.t - self.v;
        if d.abs() <= eps {
            self.v = self.t;
            return false;
        }
        self.v += d * (1.0 - (-rate * dt).exp());
        true
    }
}

#[derive(Clone, PartialEq)]
enum Mode {
    Rec,
    Busy(String),
    Done,
    Note(String, Tone),
}

struct Word {
    text: Vec<u16>,
    w: i32,
    born: Instant,
    bright: Spring,
}

/// Drawing resources, alive only while the HUD is on screen.
struct Surface {
    w: i32,
    h: i32,
    dc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    bits: *mut u32,
    mask_dc: HDC,
    mask_bmp: HBITMAP,
    old_mask_bmp: HGDIOBJ,
    mask_bits: *mut u32,
    old_font: HGDIOBJ,
    label_font: HFONT,
    text_font: HFONT,
    text_h: i32,
    label_h: i32,
    space_w: i32,
    panel_px: Vec<u32>,
    panel_key: Option<[i32; 4]>,
    cap_px: Vec<u32>,
    cap_key: Option<[i32; 3]>,
}

impl Surface {
    fn new(w: i32, h: i32, scale: f32) -> Option<Surface> {
        let dib = |dc: HDC| {
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: a w × h 32-bit DIB; bits stays valid until the bitmap is deleted.
            unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
                .ok()
                .map(|b| (b, bits.cast::<u32>()))
        };
        // SAFETY: GDI objects created here are released in Drop (or right here on failure).
        unsafe {
            let dc = CreateCompatibleDC(None);
            let mask_dc = CreateCompatibleDC(None);
            let (a, b) = (dib(dc), dib(mask_dc));
            let (Some((bmp, bits)), Some((mask_bmp, mask_bits))) = (a, b) else {
                for (bmp, _) in [a, b].into_iter().flatten() {
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                }
                let _ = DeleteDC(dc);
                let _ = DeleteDC(mask_dc);
                return None;
            };
            let old_bmp = SelectObject(dc, HGDIOBJ(bmp.0));
            let old_mask_bmp = SelectObject(mask_dc, HGDIOBJ(mask_bmp.0));
            let sc = |v: i32| (v as f32 * scale).round() as i32;
            let label_font = font(sc(LABEL_PX), FW_SEMIBOLD.0 as i32);
            let text_font = font(sc(TEXT_PX), FW_NORMAL.0 as i32);
            SetBkMode(mask_dc, TRANSPARENT);
            let old_font = SelectObject(mask_dc, HGDIOBJ(text_font.0));
            let text_h = line_height(mask_dc);
            SelectObject(mask_dc, HGDIOBJ(label_font.0));
            let label_h = line_height(mask_dc);
            let mut surf = Surface {
                w,
                h,
                dc,
                bmp,
                old_bmp,
                bits,
                mask_dc,
                mask_bmp,
                old_mask_bmp,
                mask_bits,
                old_font,
                label_font,
                text_font,
                text_h,
                label_h,
                space_w: 0,
                panel_px: vec![0; (w * h) as usize],
                panel_key: None,
                cap_px: vec![0; (w * h) as usize],
                cap_key: None,
            };
            surf.space_w = surf.measure(text_font, &[u16::from(b' ')]);
            Some(surf)
        }
    }

    fn measure(&self, font: HFONT, s: &[u16]) -> i32 {
        let mut size = SIZE::default();
        // SAFETY: valid DC, font and buffer.
        unsafe {
            SelectObject(self.mask_dc, HGDIOBJ(font.0));
            let _ = GetTextExtentPoint32W(self.mask_dc, s, &mut size);
        }
        size.cx
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: deselect then delete exactly what new() created.
        unsafe {
            SelectObject(self.mask_dc, self.old_font);
            SelectObject(self.mask_dc, self.old_mask_bmp);
            SelectObject(self.dc, self.old_bmp);
            let _ = DeleteObject(HGDIOBJ(self.label_font.0));
            let _ = DeleteObject(HGDIOBJ(self.text_font.0));
            let _ = DeleteObject(HGDIOBJ(self.mask_bmp.0));
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.mask_dc);
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Hud {
    hwnd: HWND,
    scale: f32,
    /// Screen position of the canvas once fully risen.
    pos: POINT,
    surf: Option<Surface>,
    /// Whether it should be on screen; `vis` eases towards it.
    shown: bool,
    on_screen: bool,
    vis: Spring,
    mode: Mode,
    mode_t0: Instant,
    cap_w: Spring,
    words: Vec<Word>,
    panel_vis: Spring,
    panel_h: Spring,
    scroll: Spring,
    rec_t0: Instant,
    /// Recording cut-off, while recording.
    limit: Option<Instant>,
    level: f32,
    history: [f32; BARS],
    history_t: Instant,
    bars: [Spring; BARS],
    last: Instant,
    frame_ms: u32,
}

impl Hud {
    fn new(hwnd: HWND) -> Hud {
        let now = Instant::now();
        Hud {
            hwnd,
            scale: 1.0,
            pos: POINT::default(),
            surf: None,
            shown: false,
            on_screen: false,
            vis: Spring::at(0.0),
            mode: Mode::Busy(String::new()),
            mode_t0: now,
            cap_w: Spring::at(0.0),
            words: Vec::new(),
            panel_vis: Spring::at(0.0),
            panel_h: Spring::at(0.0),
            scroll: Spring::at(0.0),
            rec_t0: now,
            limit: None,
            level: 0.0,
            history: [0.0; BARS],
            history_t: now,
            bars: [Spring::at(0.0); BARS],
            last: now,
            frame_ms: 0,
        }
    }

    fn sc(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    fn apply(&mut self, cmd: Cmd) {
        let now = Instant::now();
        match cmd {
            Cmd::Show {
                text,
                tone,
                hide_after,
                clear,
            } => {
                self.appear();
                if clear {
                    self.words.clear();
                }
                let mode = match tone {
                    Tone::Recording => Mode::Rec,
                    Tone::Busy => Mode::Busy(text),
                    Tone::Info | Tone::Error => Mode::Note(text, tone),
                };
                if mode == Mode::Rec && self.mode != Mode::Rec {
                    self.rec_t0 = now;
                    self.history = [0.0; BARS];
                    LEVEL.store(0, Ordering::Relaxed);
                }
                if mode != Mode::Rec {
                    self.limit = None;
                }
                if mode != self.mode {
                    self.mode = mode;
                    self.mode_t0 = now;
                }
                self.set_hide(hide_after);
            }
            Cmd::Words(finals, interim) => {
                if self.shown {
                    self.set_words(&finals, &interim);
                }
            }
            Cmd::Limit(at) => {
                if self.mode == Mode::Rec {
                    self.limit = Some(at);
                }
            }
            Cmd::Done => {
                self.limit = None;
                if self.shown {
                    self.mode = Mode::Done;
                    self.mode_t0 = now;
                    self.set_hide(Some(DONE_HOLD));
                }
            }
            Cmd::Hide => {
                self.set_hide(None);
                self.shown = false;
            }
        }
    }

    fn set_hide(&self, after: Option<Duration>) {
        // SAFETY: timers on our own window, on its thread.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), HIDE_TIMER);
            if let Some(d) = after {
                SetTimer(
                    Some(self.hwnd),
                    HIDE_TIMER,
                    d.as_millis().max(1) as u32,
                    None,
                );
            }
        }
    }

    /// Readies the surface on the monitor the user is looking at, if not already up.
    fn appear(&mut self) {
        self.shown = true;
        if self.surf.is_some() {
            return;
        }
        let (work, dpi) = target_monitor();
        self.scale = dpi as f32 / 96.0;
        let m = self.sc(MARGIN);
        let panel_max = 2 * self.sc(PANEL_PAD_Y) + MAX_LINES * self.sc(LINE_H);
        let w = self.sc(PANEL_W) + 2 * m;
        let h = m + panel_max + self.sc(PANEL_GAP) + self.sc(CAP_H) + m;
        self.surf = Surface::new(w, h, self.scale);
        self.pos = POINT {
            x: work.left + (work.right - work.left - w) / 2,
            y: work.bottom - self.sc(BOTTOM_GAP) - h + m,
        };
        self.vis = Spring::at(0.0);
        self.cap_w = Spring::at(0.0); // snaps to its first width
        self.panel_vis = Spring::at(0.0);
        self.panel_h = Spring::at(0.0);
        self.scroll = Spring::at(0.0);
        self.bars = [Spring::at(0.0); BARS];
        self.last = Instant::now();
    }

    fn set_words(&mut self, finals: &str, interim: &str) {
        let Some(surf) = &self.surf else { return };
        let now = Instant::now();
        let mut old = std::mem::take(&mut self.words).into_iter();
        let tokens = finals
            .split_whitespace()
            .map(|t| (t, 1.0))
            .chain(interim.split_whitespace().map(|t| (t, INTERIM)));
        for (t, bright) in tokens {
            let text: Vec<u16> = t.encode_utf16().collect();
            // Same word in the same place: keep its animation state.
            let word = match old.next() {
                Some(mut w) if w.text == text => {
                    w.bright.t = bright;
                    w
                }
                _ => Word {
                    w: surf.measure(surf.text_font, &text),
                    text,
                    born: now,
                    bright: Spring::at(bright),
                },
            };
            self.words.push(word);
        }
    }

    /// (line, x) for each word, greedy wrap at the panel's inner width; and the line count.
    fn layout(&self, space: i32) -> (Vec<(i32, i32)>, i32) {
        let width = self.sc(PANEL_W) - 2 * self.sc(PANEL_PAD_X);
        let mut out = Vec::with_capacity(self.words.len());
        let (mut line, mut x) = (0, 0);
        for w in &self.words {
            if x > 0 && x + space + w.w > width {
                line += 1;
                x = 0;
            } else if x > 0 {
                x += space;
            }
            out.push((line, x));
            x += w.w;
        }
        let lines = if self.words.is_empty() { 0 } else { line + 1 };
        (out, lines)
    }

    /// Seconds left (rounded up) once the recording limit is within `COUNTDOWN`, and
    /// when the countdown appeared.
    fn countdown(&self, now: Instant) -> Option<(u64, Instant)> {
        let left = self.limit?.checked_duration_since(now)?;
        let shown = self.limit? - COUNTDOWN;
        (self.mode == Mode::Rec && left <= COUNTDOWN)
            .then(|| (left.as_secs() + u64::from(left.subsec_nanos() > 0), shown))
    }

    /// Countdown pill: its width and digits.
    fn pill(&self, surf: &Surface, secs: u64) -> (i32, Vec<u16>) {
        let digits: Vec<u16> = secs.to_string().encode_utf16().collect();
        let tw = surf.measure(surf.label_font, &digits);
        let w = (tw as f32 + 2.0 * PILL_PAD * self.scale).max(PILL_H * self.scale);
        (w.round() as i32, digits)
    }

    /// Capsule content: (its width, label text, label offset within it).
    fn cap_content(&self, surf: &Surface, now: Instant) -> (i32, Vec<u16>, i32) {
        let utf16 = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let (off, text) = match &self.mode {
            Mode::Rec => {
                let secs = self.rec_t0.elapsed().as_secs();
                let timer = utf16(&format!("{}:{:02}", secs / 60, secs % 60));
                // Width from "0:00" so the capsule doesn't twitch as digits change.
                let tw = surf
                    .measure(surf.label_font, &utf16("0:00"))
                    .max(surf.measure(surf.label_font, &timer));
                let bars = (self.scale * (BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP))
                    .round() as i32;
                let off = self.sc(DOT) + self.sc(PART_GAP) + bars + self.sc(PART_GAP);
                let pill = self
                    .countdown(now)
                    .map_or(0, |(n, _)| self.sc(PART_GAP) + self.pill(surf, n).0);
                return (off + tw + pill, timer, off);
            }
            Mode::Busy(text) => (
                self.sc(BUSY_DOTS_W) + self.sc(PART_GAP),
                utf16(text.trim_end_matches('…')),
            ),
            Mode::Done => (self.sc(ICON) + self.sc(ICON_GAP), utf16("Pasted")),
            Mode::Note(text, _) => (self.sc(ICON) + self.sc(ICON_GAP), utf16(text)),
        };
        (off + surf.measure(surf.label_font, &text), text, off)
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.05);
        self.last = now;
        let Some(surf) = &self.surf else {
            self.schedule(0);
            return;
        };
        let (space, surf_w) = (surf.space_w, surf.w);
        let (content_w, label, label_off) = self.cap_content(surf, now);
        let mut moving = false;

        self.vis.t = if self.shown { 1.0 } else { 0.0 };
        moving |= self.vis.step(dt, 16.0, 0.004);
        if !self.shown && self.vis.v <= 0.0 {
            self.teardown();
            return;
        }

        // Waveform: fast attack, slow release; recent history spreads out from the centre.
        let rms = f32::from_bits(LEVEL.load(Ordering::Relaxed));
        let target = ((20.0 * (rms + 1e-6).log10() + 52.0) / 36.0).clamp(0.0, 1.0);
        let rate = if target > self.level { 30.0 } else { 7.0 };
        self.level += (target - self.level) * (1.0 - (-rate * dt).exp());
        if now.duration_since(self.history_t) >= Duration::from_millis(60) {
            self.history.rotate_left(1);
            self.history[BARS - 1] = self.level;
            self.history_t = now;
        }
        let centre = BARS / 2;
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let age = i.abs_diff(centre);
            bar.t = if self.mode == Mode::Rec {
                self.history[BARS - 1 - 2 * age] * (1.0 - age as f32 * 0.12)
            } else {
                0.0
            };
            bar.step(dt, 22.0, 0.002);
        }

        let (places, lines) = self.layout(space);
        self.panel_vis.t = if lines > 0 { 1.0 } else { 0.0 };
        moving |= self.panel_vis.step(dt, 14.0, 0.004);
        if lines > 0 {
            let target = 2 * self.sc(PANEL_PAD_Y) + lines.min(MAX_LINES) * self.sc(LINE_H);
            if self.panel_h.v == 0.0 {
                self.panel_h = Spring::at(target as f32);
            }
            self.panel_h.t = target as f32;
            self.scroll.t = ((lines - MAX_LINES).max(0) * self.sc(LINE_H)) as f32;
        }
        moving |= self.panel_h.step(dt, 16.0, 0.3);
        moving |= self.scroll.step(dt, 14.0, 0.3);
        // Word fades read fine at the slow rate; geometry moves at the fast one.
        let mut fading = false;
        for w in &mut self.words {
            fading |= w.bright.step(dt, 10.0, 0.004);
            fading |= now.duration_since(w.born) < WORD_IN;
        }

        let max_cap = surf_w - 2 * self.sc(MARGIN);
        self.cap_w.t = (content_w + 2 * self.sc(CAP_PAD)).min(max_cap) as f32;
        if self.cap_w.v == 0.0 {
            self.cap_w = Spring::at(self.cap_w.t);
        }
        moving |= self.cap_w.step(dt, 18.0, 0.3);
        let since_mode = now.duration_since(self.mode_t0);
        moving |= since_mode < CONTENT_IN || (self.mode == Mode::Done && since_mode < CHECK_DRAW);
        let continuous = fading || matches!(self.mode, Mode::Rec | Mode::Busy(_));

        self.draw(&places, content_w, &label, label_off, now);
        self.schedule(if moving {
            FRAME_FAST
        } else if continuous {
            FRAME_SLOW
        } else {
            0
        });
    }

    fn schedule(&mut self, ms: u32) {
        if ms == self.frame_ms {
            return;
        }
        // SAFETY: timer on our own window, on its thread.
        unsafe {
            if ms == 0 {
                let _ = KillTimer(Some(self.hwnd), FRAME_TIMER);
            } else {
                SetTimer(Some(self.hwnd), FRAME_TIMER, ms, None);
            }
        }
        self.frame_ms = ms;
    }

    fn teardown(&mut self) {
        // SAFETY: our own window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        self.on_screen = false;
        self.surf = None;
        self.words = Vec::new();
        self.schedule(0);
    }

    fn draw(
        &mut self,
        places: &[(i32, i32)],
        content_w: i32,
        label: &[u16],
        label_off: i32,
        now: Instant,
    ) {
        let s = self.scale;
        let m = self.sc(MARGIN);
        let cap_h = self.sc(CAP_H);
        let cap_w = self.cap_w.v.round() as i32;
        let pad_x = self.sc(PANEL_PAD_X);
        let pad_y = self.sc(PANEL_PAD_Y);
        let line_h = self.sc(LINE_H);
        let panel_w = self.sc(PANEL_W);
        let panel_gap = self.sc(PANEL_GAP);
        let count = self.surf.as_ref().and_then(|sf| {
            let (left, since) = self.countdown(now)?;
            let (pw, digits) = self.pill(sf, left);
            Some((left, since, pw, digits))
        });
        let Some(surf) = self.surf.as_mut() else {
            return;
        };
        let (w, h) = (surf.w, surf.h);
        let cap = (
            ((w - cap_w) / 2) as f32,
            (h - m - cap_h) as f32,
            cap_w as f32,
            cap_h as f32,
        );
        let panel_h = self.panel_h.v.round() as i32;
        let panel = (
            ((w - panel_w) / 2) as f32,
            cap.1 - (panel_gap + panel_h) as f32,
            panel_w as f32,
            panel_h as f32,
        );
        let panel_a = self.panel_vis.v;
        let panel_on = panel_a > 0.0 && panel_h > 0;

        // Shapes, each rebuilt only when its own geometry changes; the panel's fade is
        // applied while compositing so it never forces a rebuild.
        let panel_key = [panel.1 as i32, panel_h, w, h];
        if panel_on && surf.panel_key != Some(panel_key) {
            surf.panel_px.fill(0);
            shape(&mut surf.panel_px, w, h, panel, PANEL_RADIUS * s, 1.0, s);
            surf.panel_key = Some(panel_key);
        }
        let cap_key = [cap.0 as i32, cap_w, w];
        if surf.cap_key != Some(cap_key) {
            surf.cap_px.fill(0);
            shape(&mut surf.cap_px, w, h, cap, cap.3 / 2.0, 1.0, s);
            surf.cap_key = Some(cap_key);
        }
        // SAFETY: both DIBs are w × h 32-bit pixels, alive while surf is; GDI is done with
        // them (GdiFlush below runs before the mask is read).
        let (px, mask) = unsafe {
            (
                std::slice::from_raw_parts_mut(surf.bits, (w * h) as usize),
                std::slice::from_raw_parts(surf.mask_bits, (w * h) as usize),
            )
        };
        if panel_on {
            let pa = (panel_a.min(1.0) * 256.0) as u32;
            for ((out, &p), &c) in px.iter_mut().zip(&surf.panel_px).zip(&surf.cap_px) {
                *out = if c >> 24 == 255 || p == 0 {
                    c
                } else {
                    under(c, scale_px(p, pa))
                };
            }
        } else {
            px.copy_from_slice(&surf.cap_px);
        }

        let grey = |a: f32| {
            let g = (a.clamp(0.0, 1.0) * 255.0).round() as u32;
            COLORREF(g | (g << 8) | (g << 16))
        };
        let content_a = ease(now.duration_since(self.mode_t0), CONTENT_IN);
        let x0 = cap.0 as i32 + (cap_w - content_w) / 2;
        let cy = cap.1 + cap.3 / 2.0;

        // Text mask.
        // SAFETY: drawing into our own memory DC.
        unsafe {
            for (l, t, r, b) in [rect_i(panel), rect_i(cap)] {
                let _ = PatBlt(surf.mask_dc, l, t, r - l, b - t, BLACKNESS);
            }
            if panel_on {
                SelectObject(surf.mask_dc, HGDIOBJ(surf.text_font.0));
                IntersectClipRect(
                    surf.mask_dc,
                    panel.0 as i32 + 1,
                    panel.1 as i32 + 1,
                    (panel.0 + panel.2) as i32 - 1,
                    (panel.1 + panel.3) as i32 - 1,
                );
                let top = panel.1 as i32 + pad_y - self.scroll.v.round() as i32;
                let first = ((panel.1 as i32 - top) / line_h - 1).max(0);
                for (word, &(line, x)) in self.words.iter().zip(places) {
                    if line < first {
                        continue;
                    }
                    let t = ease(now.duration_since(word.born), WORD_IN);
                    let rise = ((1.0 - t) * 4.0 * s).round() as i32;
                    let y = top + line * line_h + (line_h - surf.text_h) / 2 + rise;
                    SetTextColor(surf.mask_dc, grey(t * word.bright.v * panel_a));
                    let _ = TextOutW(surf.mask_dc, panel.0 as i32 + pad_x + x, y, &word.text);
                }
                SelectClipRgn(surf.mask_dc, None);
            }
            SelectObject(surf.mask_dc, HGDIOBJ(surf.label_font.0));
            IntersectClipRect(
                surf.mask_dc,
                cap.0 as i32,
                cap.1 as i32,
                (cap.0 + cap.2) as i32,
                (cap.1 + cap.3) as i32,
            );
            let label_a = if self.mode == Mode::Rec { 0.62 } else { 0.95 };
            SetTextColor(surf.mask_dc, grey(label_a * content_a));
            let _ = TextOutW(
                surf.mask_dc,
                x0 + label_off,
                cy as i32 - surf.label_h / 2,
                label,
            );
            SelectClipRgn(surf.mask_dc, None);
            let _ = GdiFlush();
        }

        // Text: the panel (with a top fade once it scrolls) and the capsule.
        if panel_on {
            let fade_in = (self.scroll.v / line_h as f32).min(1.0);
            let text_top = panel.1 + pad_y as f32 * 0.6;
            let fade_len = line_h as f32 * 1.1;
            blend_text(px, mask, w, rect_i(panel), INK, |y| {
                let t = ((y - text_top) / fade_len).clamp(0.0, 1.0);
                1.0 - fade_in * (1.0 - t * t * (3.0 - 2.0 * t))
            });
        }
        blend_text(px, mask, w, rect_i(cap), INK, |_| 1.0);

        // Recording limit countdown: a tinted pill at the capsule's right end.
        if let Some((left, since, pw, digits)) = count {
            let colour = if left <= COUNTDOWN_RED { RED } else { AMBER };
            let a = ease(now.saturating_duration_since(since), CONTENT_IN) * content_a;
            let ph = PILL_H * s;
            let rect = ((x0 + content_w - pw) as f32, cy - ph / 2.0, pw as f32, ph);
            paint(
                px,
                w,
                h,
                (
                    rect.0 - 1.0,
                    rect.1 - 1.0,
                    rect.0 + rect.2 + 1.0,
                    rect.1 + ph + 1.0,
                ),
                |x, y| {
                    let d = rounded_rect_sd(x, y, rect, ph / 2.0);
                    (colour, 0.22 * a * (0.5 - d).clamp(0.0, 1.0))
                },
            );
            let tw = surf.measure(surf.label_font, &digits);
            // SAFETY: drawing into our own memory DC; the pill area is still black there.
            unsafe {
                SelectObject(surf.mask_dc, HGDIOBJ(surf.label_font.0));
                SetTextColor(surf.mask_dc, grey(a));
                let _ = TextOutW(
                    surf.mask_dc,
                    rect.0 as i32 + (pw - tw) / 2,
                    cy as i32 - surf.label_h / 2,
                    &digits,
                );
                let _ = GdiFlush();
            }
            // SAFETY: as above; GDI is flushed.
            let mask = unsafe { std::slice::from_raw_parts(surf.mask_bits, (w * h) as usize) };
            blend_text(px, mask, w, rect_i(rect), colour, |_| 1.0);
        }

        // Capsule glyphs.
        let x0 = x0 as f32;
        let secs = now.duration_since(self.mode_t0).as_secs_f32();
        match &self.mode {
            Mode::Rec => {
                let breathe = 0.5 + 0.5 * (secs * std::f32::consts::TAU / 1.6).cos();
                disc(
                    px,
                    w,
                    h,
                    (x0 + DOT as f32 * s / 2.0, cy),
                    DOT as f32 * s / 2.0,
                    RED,
                    (0.55 + 0.45 * breathe) * content_a,
                );
                let bx0 = x0 + (DOT + PART_GAP) as f32 * s;
                let bw = (BAR_W * s).max(2.0);
                for (i, bar) in self.bars.iter().enumerate() {
                    let bh = bw + bar.v * (BAR_MAX * s - bw);
                    let bx = bx0 + i as f32 * (BAR_W + BAR_GAP) * s;
                    let rect = (bx, cy - bh / 2.0, bw, bh);
                    paint(
                        px,
                        w,
                        h,
                        (bx - 1.0, rect.1 - 1.0, bx + bw + 1.0, rect.1 + bh + 1.0),
                        |x, y| {
                            let d = rounded_rect_sd(x, y, rect, bw / 2.0);
                            (INK, 0.92 * content_a * (0.5 - d).clamp(0.0, 1.0))
                        },
                    );
                }
            }
            Mode::Busy(_) => {
                for i in 0..3 {
                    let phase = secs * std::f32::consts::TAU / 1.1 - i as f32 * 0.7;
                    let wave = 0.5 + 0.5 * phase.sin();
                    let c = (x0 + (2.5 + i as f32 * 9.0) * s, cy - wave * 2.0 * s);
                    disc(px, w, h, c, 2.5 * s, INK, (0.35 + 0.6 * wave) * content_a);
                }
            }
            Mode::Done => {
                let progress = ease(now.duration_since(self.mode_t0), CHECK_DRAW);
                icon(px, w, h, (x0, cy), s, GREEN, content_a, |x, y| {
                    check_sd(x, y, progress) - 1.1
                });
            }
            Mode::Note(_, tone) => {
                let (colour, flip) = if *tone == Tone::Error {
                    (AMBER, 1.0)
                } else {
                    (BLUE, -1.0)
                };
                icon(px, w, h, (x0, cy), s, colour, content_a, |x, y| {
                    // "!" (or, upside down, "i"): a bar and a dot.
                    let y = y * flip;
                    let bar = segment_sd(x, y, (0.0, -3.6), (0.0, 0.9)) - 0.95;
                    let dot = x.hypot_fast(y - 3.4) - 1.1;
                    bar.min(dot)
                });
            }
        }

        // SAFETY: presenting our memory DC to our own layered window.
        unsafe {
            let dst = POINT {
                x: self.pos.x,
                y: self.pos.y + ((1.0 - self.vis.v) * RISE * s).round() as i32,
            };
            let size = SIZE { cx: w, cy: h };
            let src = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: (self.vis.v.clamp(0.0, 1.0) * 255.0).round() as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if let Err(e) = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst),
                Some(&size),
                Some(surf.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                log::warn!("overlay update: {e}");
            }
            if !self.on_screen {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                self.on_screen = true;
            }
        }
    }
}

/// Ease-out cubic over `total`.
fn ease(d: Duration, total: Duration) -> f32 {
    let t = (d.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Work area and dpi of the monitor the user is looking at: the foreground window's,
/// else the cursor's.
fn target_monitor() -> (RECT, u32) {
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

fn line_height(hdc: HDC) -> i32 {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: valid DC.
    let _ = unsafe { GetTextMetricsW(hdc, &mut tm) };
    tm.tmHeight
}

fn rect_i((x, y, w, h): (f32, f32, f32, f32)) -> (i32, i32, i32, i32) {
    (
        x as i32,
        y as i32,
        (x + w).ceil() as i32,
        (y + h).ceil() as i32,
    )
}

/// Signed distance from (px, py) to a rounded rectangle; negative inside.
fn rounded_rect_sd(px: f32, py: f32, (x, y, w, h): (f32, f32, f32, f32), r: f32) -> f32 {
    let r = r.min(w / 2.0).min(h / 2.0);
    let qx = (px - x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (py - y - h / 2.0).abs() - (h / 2.0 - r);
    qx.max(0.0).hypot_fast(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

fn segment_sd(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let (pax, pay) = (px - a.0, py - a.1);
    let len2 = (bax * bax + bay * bay).max(1e-6);
    let t = ((pax * bax + pay * bay) / len2).clamp(0.0, 1.0);
    (pax - bax * t).hypot_fast(pay - bay * t)
}

/// Distance to a check mark drawn up to `progress` (0..1), in 96-dpi icon units.
fn check_sd(x: f32, y: f32, progress: f32) -> f32 {
    let pts: [(f32, f32); 3] = [(-3.6, 0.1), (-1.1, 2.6), (3.8, -2.4)];
    let lens = [
        (pts[1].0 - pts[0].0).hypot_fast(pts[1].1 - pts[0].1),
        (pts[2].0 - pts[1].0).hypot_fast(pts[2].1 - pts[1].1),
    ];
    let mut left = progress * (lens[0] + lens[1]);
    let mut d = f32::MAX;
    for i in 0..2 {
        if left <= 0.0 {
            break;
        }
        let f = (left / lens[i]).min(1.0);
        let (a, b) = (pts[i], pts[i + 1]);
        let end = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
        d = d.min(segment_sd(x, y, a, end));
        left -= lens[i];
    }
    d
}

/// Scales a premultiplied pixel by `k`/256.
fn scale_px(p: u32, k: u32) -> u32 {
    let rb = (((p & 0x00FF_00FF) * k) >> 8) & 0x00FF_00FF;
    let ga = ((((p >> 8) & 0x00FF_00FF) * k) >> 8) & 0x00FF_00FF;
    rb | (ga << 8)
}

/// Premultiplied `top` over premultiplied `bottom`.
fn under(top: u32, bottom: u32) -> u32 {
    let inv = 256 - (top >> 24) - ((top >> 24) >> 7);
    top + scale_px(bottom, inv)
}

/// Premultiplied "over": (colour, alpha) onto a premultiplied BGRA pixel.
fn over(dst: u32, c: Rgb, a: f32) -> u32 {
    if a <= 0.0 {
        return dst;
    }
    let a = (a.min(1.0) * 255.0 + 0.5) as u32;
    let inv = 255 - a;
    // x / 255 for x ≤ 255², rounded.
    let div = |x: u32| (x + 128 + ((x + 128) >> 8)) >> 8;
    let ch = |shift: u32, src: u8| div(u32::from(src) * a + ((dst >> shift) & 0xFF) * inv) << shift;
    (div(255 * a + (dst >> 24) * inv) << 24) | ch(16, c.0) | ch(8, c.1) | ch(0, c.2)
}

/// `f32::hypot` guards against overflow and is slow; these values are small.
trait HypotFast {
    fn hypot_fast(self, other: f32) -> f32;
}

impl HypotFast for f32 {
    fn hypot_fast(self, other: f32) -> f32 {
        (self * self + other * other).sqrt()
    }
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let l = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

/// Calls `f` for each pixel centre in the box and blends the (colour, alpha) it returns.
fn paint(
    px: &mut [u32],
    w: i32,
    h: i32,
    bbox: (f32, f32, f32, f32),
    f: impl Fn(f32, f32) -> (Rgb, f32),
) {
    paint_raw(px, w, h, bbox, |x, y, dst| {
        let (c, a) = f(x, y);
        over(dst, c, a)
    });
}

/// Calls `f` for each pixel centre in the box with the pixel, and stores what it returns.
fn paint_raw(
    px: &mut [u32],
    w: i32,
    h: i32,
    (x0, y0, x1, y1): (f32, f32, f32, f32),
    f: impl Fn(f32, f32, u32) -> u32,
) {
    let (xa, xb) = ((x0.floor() as i32).max(0), (x1.ceil() as i32).min(w));
    let (ya, yb) = ((y0.floor() as i32).max(0), (y1.ceil() as i32).min(h));
    for yy in ya..yb {
        for xx in xa..xb {
            let i = (yy * w + xx) as usize;
            px[i] = f(xx as f32 + 0.5, yy as f32 + 0.5, px[i]);
        }
    }
}

fn disc(px: &mut [u32], w: i32, h: i32, c: (f32, f32), r: f32, colour: Rgb, alpha: f32) {
    let bbox = (c.0 - r - 1.0, c.1 - r - 1.0, c.0 + r + 1.0, c.1 + r + 1.0);
    paint(px, w, h, bbox, |x, y| {
        let d = (x - c.0).hypot_fast(y - c.1) - r;
        (colour, alpha * (0.5 - d).clamp(0.0, 1.0))
    });
}

/// A 16 px coloured disc at the start of the content with a dark mark knocked into it;
/// `mark` is a distance field in 96-dpi units around the disc's centre.
#[allow(clippy::too_many_arguments)]
fn icon(
    px: &mut [u32],
    w: i32,
    h: i32,
    (x0, cy): (f32, f32),
    s: f32,
    colour: Rgb,
    alpha: f32,
    mark: impl Fn(f32, f32) -> f32,
) {
    let r = ICON as f32 * s / 2.0;
    let c = (x0 + r, cy);
    let bbox = (c.0 - r - 1.0, c.1 - r - 1.0, c.0 + r + 1.0, c.1 + r + 1.0);
    paint(px, w, h, bbox, |x, y| {
        let cover = (0.5 - ((x - c.0).hypot_fast(y - c.1) - r)).clamp(0.0, 1.0);
        if cover <= 0.0 {
            return (colour, 0.0);
        }
        let ink = (0.5 - mark((x - c.0) / s, (y - c.1) / s) * s).clamp(0.0, 1.0);
        (lerp(colour, FILL_BOTTOM, ink), alpha * cover)
    });
}

/// Shadow, gradient fill and hairlines of one rounded shape.
fn shape(buf: &mut [u32], w: i32, h: i32, rect: (f32, f32, f32, f32), r: f32, alpha: f32, s: f32) {
    let (blur, dy) = (18.0 * s, 6.0 * s);
    let (tight, tdy) = (3.0 * s, 1.0 * s);
    let far = (rect.0, rect.1 + dy, rect.2, rect.3);
    let near = (rect.0, rect.1 + tdy, rect.2, rect.3);
    let bbox = (
        rect.0 - blur,
        rect.1 - blur,
        rect.0 + rect.2 + blur,
        rect.1 + rect.3 + dy + blur,
    );
    // A smooth bump that reaches zero at 1.4 × the blur: no exp, and a hard edge to skip.
    let falloff = |d: f32, b: f32| {
        let x = (d.max(0.0) / (1.4 * b)).min(1.0);
        let k = 1.0 - x * x;
        k * k * k
    };
    let reach = 1.4 * blur + dy;
    // Deep inside, a pixel is the row's fill over full shadow: precompute per row.
    let rows: Vec<u32> = (0..h)
        .map(|y| {
            let t = ((y as f32 + 0.5 - rect.1) / rect.3).clamp(0.0, 1.0);
            let under = over(
                over(0, Rgb(0, 0, 0), 0.30 * alpha),
                Rgb(0, 0, 0),
                0.22 * alpha,
            );
            over(under, lerp(FILL_TOP, FILL_BOTTOM, t), FILL_ALPHA * alpha)
        })
        .collect();
    paint_raw(buf, w, h, bbox, |x, y, mut out| {
        let d = rounded_rect_sd(x, y, rect, r);
        if d > reach {
            return out;
        }
        if d < -1.5 {
            return rows[y as usize];
        }
        let t = ((y - rect.1) / rect.3).clamp(0.0, 1.0);
        out = over(
            out,
            Rgb(0, 0, 0),
            0.30 * alpha * falloff(rounded_rect_sd(x, y, far, r), blur),
        );
        if d < 1.4 * tight + tdy {
            out = over(
                out,
                Rgb(0, 0, 0),
                0.22 * alpha * falloff(rounded_rect_sd(x, y, near, r), tight),
            );
        }
        let cover = (0.5 - d).clamp(0.0, 1.0);
        // A dark outer hairline keeps the edge crisp over dark backgrounds too.
        let outer = (1.5 - d).clamp(0.0, 1.0) - cover;
        out = over(out, Rgb(0, 0, 0), 0.45 * alpha * outer);
        if cover > 0.0 {
            out = over(
                out,
                lerp(FILL_TOP, FILL_BOTTOM, t),
                FILL_ALPHA * alpha * cover,
            );
            // Inner hairline, brighter at the top: reads as a lit edge.
            let inner = cover - (-0.5 - d).clamp(0.0, 1.0);
            out = over(out, Rgb(255, 255, 255), (0.16 - 0.11 * t) * alpha * inner);
        }
        out
    });
}

/// Blends ink through the text mask inside the box; `fade(y)` scales it per row.
fn blend_text(
    px: &mut [u32],
    mask: &[u32],
    w: i32,
    (x0, y0, x1, y1): (i32, i32, i32, i32),
    colour: Rgb,
    fade: impl Fn(f32) -> f32,
) {
    let h = (px.len() as i32) / w;
    for yy in y0.max(0)..y1.min(h) {
        let f = fade(yy as f32 + 0.5);
        if f <= 0.0 {
            continue;
        }
        for xx in x0.max(0)..x1.min(w) {
            let i = (yy * w + xx) as usize;
            let g = (mask[i] >> 8) & 0xFF; // green channel
            if g != 0 {
                px[i] = over(px[i], colour, g as f32 / 255.0 * f);
            }
        }
    }
}
