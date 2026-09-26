//! The settings window: hotkey, language, sounds, autostart, API key, dictionary and the
//! OpenWhispr import. Created on demand on the ipc thread; Save sends `UiCmd`s to core.

use super::ui::{self, px, rect};
use crate::event::{Event, UiCmd};
use crate::hotkey::Chord;
use crate::settings::Settings;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{BST_CHECKED, EM_SETCUEBANNER};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DestroyWindow, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_PASSWORD,
    ES_WANTRETURN, IDC_ARROW, IDCANCEL, IDOK, IsDialogMessageW, LoadCursorW, MB_ICONWARNING, MB_OK,
    MSG, MessageBoxW, PostMessageW, RegisterClassW, SW_SHOW, SWP_NOMOVE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowPos, ShowWindow, WM_CLOSE, WM_COMMAND, WM_DESTROY, WNDCLASSW,
    WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_MINIMIZEBOX, WS_SYSMENU, WS_VSCROLL,
};
use windows::core::{HSTRING, PCWSTR, w};

const CLASS: PCWSTR = w!("gemdict.settings");

const ID_HOTKEY: usize = 200;
const ID_LANGUAGE: usize = 201;
const ID_SOUNDS: usize = 202;
const ID_AUTOSTART: usize = 203;
const ID_KEY: usize = 204;
const ID_TEST: usize = 205;
const ID_IMPORT: usize = 206;
const ID_WORDS: usize = 207;

static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
static HWND_: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy)]
struct Ctl {
    hotkey: HWND,
    language: HWND,
    sounds: HWND,
    autostart: HWND,
    key: HWND,
    words: HWND,
}

struct State {
    font: HFONT,
    ctl: Ctl,
    loaded: Settings,
    loaded_words: Vec<String>,
    loaded_autostart: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Settings file and database paths.
pub fn init(settings: PathBuf, db: PathBuf) {
    let _ = PATHS.set((settings, db));
}

pub fn pre_translate(msg: &MSG) -> bool {
    let h = HWND_.load(Ordering::Acquire);
    // SAFETY: our own window on this thread.
    h != 0 && unsafe { IsDialogMessageW(HWND(h as *mut _), msg) }.as_bool()
}

pub fn show() {
    let h = HWND_.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: our own window.
        let _ = unsafe { SetForegroundWindow(HWND(h as *mut _)) };
        return;
    }
    if let Err(e) = create() {
        log::error!("settings window: {e}");
    }
}

fn label(parent: HWND, text: &str, font: HFONT) -> HWND {
    // Static text isn't a tab stop, but WS_TABSTOP on a static is ignored by the dialog manager.
    ui::child(parent, w!("STATIC"), text, 0, 0, 0, font)
}

fn create() -> windows::core::Result<()> {
    let Some((settings_path, db)) = PATHS.get() else {
        return Ok(());
    };
    let loaded = Settings::load(settings_path);
    let loaded_words = crate::store::Store::open_read(db)
        .and_then(|s| s.dictionary())
        .unwrap_or_default();
    let loaded_autostart = crate::autostart::enabled();

    // SAFETY: standard class registration and creation.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as usize as *mut _,
            ),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            w!("gemdict settings"),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    let dpi = ui::dpi(hwnd);
    let font = ui::font(dpi);
    let p = |v| px(v, dpi);
    let (m, row, lw, fw) = (p(12), p(24), p(130), p(300));
    let edit = w!("EDIT");
    let button = w!("BUTTON");
    let mut y = m;
    let mut field = |text: &str, style: u32, id: usize, value: &str| {
        ui::place(label(hwnd, text, font), rect(m, y + p(4), lw, row));
        let h = ui::child(
            hwnd,
            edit,
            value,
            ES_AUTOHSCROLL as u32 | style,
            WS_EX_CLIENTEDGE.0,
            id,
            font,
        );
        ui::place(h, rect(m + lw, y, fw, row));
        y += row + p(8);
        h
    };
    let hotkey = field("Hotkey", 0, ID_HOTKEY, &loaded.hotkey);
    let language = field("Language", 0, ID_LANGUAGE, &loaded.language);
    let key = field("Gemini API key", ES_PASSWORD as u32, ID_KEY, "");
    let cue = HSTRING::from("Leave blank to keep the saved key");
    ui::send(key, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);

    let test = ui::child(hwnd, button, "Test key", 0, 0, ID_TEST, font);
    ui::place(test, rect(m + lw, y, p(110), row));
    let import = ui::child(
        hwnd,
        button,
        "Import from OpenWhispr",
        0,
        0,
        ID_IMPORT,
        font,
    );
    ui::place(import, rect(m + lw + p(120), y, p(180), row));
    y += row + p(12);

    let checkbox = |text: &str, id: usize, on: bool, y: i32| {
        let h = ui::child(hwnd, button, text, BS_AUTOCHECKBOX as u32, 0, id, font);
        ui::place(h, rect(m + lw, y, fw, row));
        ui::send(
            h,
            BM_SETCHECK,
            if on { BST_CHECKED.0 as usize } else { 0 },
            0,
        );
        h
    };
    let sounds = checkbox("Start/stop sounds", ID_SOUNDS, loaded.sounds, y);
    y += row;
    let autostart = checkbox("Start with Windows", ID_AUTOSTART, loaded_autostart, y);
    y += row + p(12);

    ui::place(
        label(hwnd, "Dictionary\n(one per line)", font),
        rect(m, y, lw, row * 2),
    );
    let words = ui::child(
        hwnd,
        edit,
        &loaded_words.join("\r\n"),
        (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | WS_VSCROLL.0,
        WS_EX_CLIENTEDGE.0,
        ID_WORDS,
        font,
    );
    ui::place(words, rect(m + lw, y, fw, p(160)));
    y += p(160) + p(14);

    let bw = p(90);
    let right = m + lw + fw;
    let save = ui::child(
        hwnd,
        button,
        "Save",
        BS_DEFPUSHBUTTON as u32,
        0,
        IDOK.0 as usize,
        font,
    );
    ui::place(save, rect(right - 2 * bw - p(8), y, bw, row + p(2)));
    let cancel = ui::child(hwnd, button, "Cancel", 0, 0, IDCANCEL.0 as usize, font);
    ui::place(cancel, rect(right - bw, y, bw, row + p(2)));
    y += row + p(2) + m;

    // SAFETY: sizing and showing our own window. The frame is added to the client size.
    unsafe {
        use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
        let mut r = rect(0, 0, right + m, y);
        let _ = AdjustWindowRectExForDpi(
            &mut r,
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            false,
            WS_EX_CONTROLPARENT,
            dpi,
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            font,
            ctl: Ctl {
                hotkey,
                language,
                sounds,
                autostart,
                key,
                words,
            },
            loaded,
            loaded_words,
            loaded_autostart,
        })
    });
    HWND_.store(hwnd.0 as isize, Ordering::Release);
    Ok(())
}

fn send_core(cmd: UiCmd) {
    super::ipc::send(Event::Ui(cmd));
}

fn checked(h: HWND) -> bool {
    ui::send(h, BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
}

/// Validates and sends changes. Returns false to keep the window open.
fn save(hwnd: HWND) -> bool {
    // Copy out first: MessageBoxW and control messages re-enter wndproc, so no borrow may be
    // held across them.
    let Some((c, loaded, loaded_words, loaded_autostart)) = STATE.with(|s| {
        s.borrow().as_ref().map(|s| {
            (
                s.ctl,
                s.loaded.clone(),
                s.loaded_words.clone(),
                s.loaded_autostart,
            )
        })
    }) else {
        return true;
    };
    let hotkey = ui::get_text(c.hotkey);
    let Some(chord) = Chord::parse(&hotkey) else {
        let msg = HSTRING::from(format!(
            "\"{}\" isn't a hotkey gemdict understands. Try Ctrl+Win or Ctrl+Alt+Space.",
            hotkey.trim()
        ));
        // SAFETY: modal box owned by our window.
        unsafe { MessageBoxW(Some(hwnd), &msg, w!("gemdict"), MB_OK | MB_ICONWARNING) };
        return false;
    };
    let new = Settings {
        hotkey: chord.to_string(),
        language: ui::get_text(c.language).trim().to_string(),
        sounds: checked(c.sounds),
    };
    if new != loaded {
        send_core(UiCmd::SaveSettings(new));
    }
    let words: Vec<String> = ui::get_text(c.words)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if words != loaded_words {
        send_core(UiCmd::SetDictionary(words));
    }
    let autostart = checked(c.autostart);
    if autostart != loaded_autostart {
        send_core(UiCmd::SetAutostart(autostart));
    }
    let key = ui::get_text(c.key);
    ui::set_text(c.key, "");
    if !key.trim().is_empty() {
        send_core(UiCmd::SetApiKey(key.trim().to_string()));
    }
    true
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            match id {
                x if x == IDOK.0 as usize => {
                    if save(hwnd) {
                        // SAFETY: our own window.
                        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                    }
                }
                x if x == IDCANCEL.0 as usize => {
                    // SAFETY: our own window.
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                }
                ID_TEST => send_core(UiCmd::TestKey),
                ID_IMPORT => send_core(UiCmd::ImportOpenWhispr),
                _ => {}
            }
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => {
            HWND_.store(0, Ordering::Release);
            if let Some(s) = STATE.with(|s| s.borrow_mut().take()) {
                // SAFETY: the controls go with the window.
                let _ = unsafe { DeleteObject(s.font.into()) };
            }
        }
        // SAFETY: default handling.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
