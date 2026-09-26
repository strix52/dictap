//! The history window: search, a virtual list of dictations, the selected one's full text,
//! and Copy / Retry / Delete. Plain Win32 on the ipc thread, created on demand and destroyed
//! on close so it costs nothing while hidden. Reads through its own read-only connection;
//! every change goes to core as a `UiCmd`.

use super::ui::{self, px, rect};
use crate::event::{Event, UiCmd};
use crate::store::{self, Row, Store};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, DeleteObject, HBRUSH, HFONT, InvalidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    EM_SETCUEBANNER, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
    LIST_VIEW_ITEM_STATE_FLAGS, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_FOCUSED,
    LVIS_SELECTED, LVITEMW, LVM_ENSUREVISIBLE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW,
    LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVM_SETITEMSTATE,
    LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVN_KEYDOWN, LVNI_SELECTED, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER, LVS_OWNERDATA, LVS_REPORT, LVS_SHOWSELALWAYS,
    LVS_SINGLESEL, NM_DBLCLK, NMHDR, NMLVDISPINFOW, NMLVKEYDOWN, SetWindowTheme, WC_LISTVIEWW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, VK_CONTROL, VK_DELETE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    EN_CHANGE, ES_AUTOHSCROLL, ES_MULTILINE, ES_READONLY, GetClientRect, IDC_ARROW, IDCANCEL,
    IDYES, IsDialogMessageW, IsIconic, KillTimer, LoadCursorW, MB_ICONWARNING, MB_YESNO,
    MINMAXINFO, MSG, MessageBoxW, PostMessageW, RegisterClassW, SW_RESTORE, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_GETMINMAXINFO,
    WM_NOTIFY, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT,
    WS_OVERLAPPEDWINDOW, WS_VSCROLL,
};
use windows::core::{HSTRING, PWSTR, w};

const CLASS: windows::core::PCWSTR = w!("gemdict.history");
/// Posted (from any thread) when rows change.
const WM_APP_CHANGED: u32 = WM_APP + 10;
const LIMIT: usize = 5000;
const SEARCH_TIMER: usize = 1;

const ID_SEARCH: usize = 100;
const ID_LIST: usize = 101;
const ID_DETAIL: usize = 102;
const ID_COPY: usize = 103;
const ID_RETRY: usize = 104;
const ID_DELETE: usize = 105;
const ID_SETTINGS: usize = 106;

static DB: OnceLock<PathBuf> = OnceLock::new();
static HWND_: AtomicIsize = AtomicIsize::new(0);

/// Control handles. Copied out before any message is sent: list views call back into
/// `wndproc` synchronously, so nothing may stay borrowed across a `SendMessage`.
#[derive(Clone, Copy)]
struct Ctl {
    hwnd: HWND,
    font: HFONT,
    search: HWND,
    list: HWND,
    detail: HWND,
    copy: HWND,
    retry: HWND,
    delete: HWND,
    settings: HWND,
}

#[derive(Default)]
struct Data {
    store: Option<Store>,
    rows: Vec<Row>,
    /// The cell the list view is reading, kept alive until the next request.
    cell: Vec<u16>,
}

thread_local! {
    static CTL: Cell<Option<Ctl>> = const { Cell::new(None) };
    static DATA: RefCell<Data> = RefCell::new(Data::default());
}

fn ctl() -> Option<Ctl> {
    CTL.with(Cell::get)
}

/// Brief access to the rows. Never send window messages inside `f`.
fn data<R>(f: impl FnOnce(&mut Data) -> R) -> R {
    DATA.with(|d| f(&mut d.borrow_mut()))
}

fn row_at(i: usize) -> Option<Row> {
    data(|d| d.rows.get(i).cloned())
}

pub fn init(db: PathBuf) {
    let _ = DB.set(db);
}

/// From any thread: refresh the window if it's open.
pub fn changed() {
    let h = HWND_.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: a stale handle just fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(h as *mut _)),
                WM_APP_CHANGED,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

/// Lets Tab/Enter/Escape work between controls. Call from the ipc message loop.
pub fn pre_translate(msg: &MSG) -> bool {
    let h = HWND_.load(Ordering::Acquire);
    // SAFETY: our own window on this thread.
    h != 0 && unsafe { IsDialogMessageW(HWND(h as *mut _), msg) }.as_bool()
}

/// Shows (creating if needed) and focuses the window. ipc thread only.
pub fn show() {
    if let Some(c) = ctl() {
        // SAFETY: our own window.
        unsafe {
            if IsIconic(c.hwnd).as_bool() {
                let _ = ShowWindow(c.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(c.hwnd);
        }
        return;
    }
    if let Err(e) = create() {
        log::error!("history window: {e}");
    }
}

fn create() -> windows::core::Result<()> {
    // SAFETY: standard class registration (repeat registration fails harmlessly) and creation.
    let hwnd = unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            w!("gemdict history"),
            WS_OVERLAPPEDWINDOW,
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
    let store = DB.get().and_then(|p| match Store::open_read(p) {
        Ok(s) => Some(s),
        Err(e) => {
            log::error!("history read connection: {e}");
            None
        }
    });
    data(|d| {
        *d = Data {
            store,
            ..Default::default()
        }
    });
    let c = build(hwnd, dpi);
    CTL.with(|x| x.set(Some(c)));
    HWND_.store(hwnd.0 as isize, Ordering::Release);
    // SAFETY: sizing our own window.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            px(860, dpi),
            px(600, dpi),
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    layout(hwnd);
    reload(true);
    // SAFETY: showing and focusing our own window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(c.search));
    }
    Ok(())
}

fn build(hwnd: HWND, dpi: u32) -> Ctl {
    let font = ui::font(dpi);
    let edit = w!("EDIT");
    let button = w!("BUTTON");
    let search = ui::child(
        hwnd,
        edit,
        "",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE.0,
        ID_SEARCH,
        font,
    );
    let cue = HSTRING::from("Search history");
    ui::send(search, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
    let settings = ui::child(hwnd, button, "Settings…", 0, 0, ID_SETTINGS, font);
    let list = ui::child(
        hwnd,
        WC_LISTVIEWW,
        "",
        LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER,
        WS_EX_CLIENTEDGE.0,
        ID_LIST,
        font,
    );
    // SAFETY: theming our own control.
    let _ = unsafe { SetWindowTheme(list, w!("Explorer"), None) };
    let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
    ui::send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
    for (i, (name, width)) in [("When", 130), ("Text", 480), ("Status", 150)]
        .iter()
        .enumerate()
    {
        let name = HSTRING::from(*name);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            cx: px(*width, dpi),
            pszText: PWSTR(name.as_ptr().cast_mut()),
            ..Default::default()
        };
        ui::send(
            list,
            LVM_INSERTCOLUMNW,
            i,
            std::ptr::from_ref(&col) as isize,
        );
    }
    let detail = ui::child(
        hwnd,
        edit,
        "",
        (ES_MULTILINE | ES_READONLY) as u32 | WS_VSCROLL.0,
        WS_EX_CLIENTEDGE.0,
        ID_DETAIL,
        font,
    );
    let copy = ui::child(hwnd, button, "Copy", 0, 0, ID_COPY, font);
    let retry = ui::child(hwnd, button, "Retry", 0, 0, ID_RETRY, font);
    let delete = ui::child(hwnd, button, "Delete", 0, 0, ID_DELETE, font);
    Ctl {
        hwnd,
        font,
        search,
        list,
        detail,
        copy,
        retry,
        delete,
        settings,
    }
}

fn layout(hwnd: HWND) {
    let Some(c) = ctl() else {
        return;
    };
    let mut rc = RECT::default();
    // SAFETY: plain query.
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let dpi = ui::dpi(hwnd);
    let (w, h) = (rc.right, rc.bottom);
    let m = px(10, dpi);
    let row = px(26, dpi);
    let bw = px(96, dpi);
    let detail_h = px(110, dpi);
    ui::place(c.search, rect(m, m, w - 3 * m - bw, row));
    ui::place(c.settings, rect(w - m - bw, m, bw, row));
    let list_top = m + row + m;
    let buttons_top = h - m - row;
    let detail_top = buttons_top - m - detail_h;
    ui::place(
        c.list,
        rect(m, list_top, w - 2 * m, detail_top - m - list_top),
    );
    ui::place(c.detail, rect(m, detail_top, w - 2 * m, detail_h));
    ui::place(c.copy, rect(m, buttons_top, bw, row));
    ui::place(c.retry, rect(m + bw + m, buttons_top, bw, row));
    ui::place(c.delete, rect(w - m - bw, buttons_top, bw, row));
    // The text column takes what the others leave.
    let fixed = px(130, dpi) + px(150, dpi) + px(24, dpi);
    let text_w = (w - 2 * m - fixed).max(px(120, dpi));
    ui::send(c.list, LVM_SETCOLUMNWIDTH, 1, text_w as isize);
}

fn selected(c: &Ctl) -> Option<usize> {
    let i = ui::send(c.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    (i >= 0 && (i as usize) < data(|d| d.rows.len())).then_some(i as usize)
}

/// Selects item `i`, or clears the selection with `usize::MAX`.
fn select(c: &Ctl, i: usize) {
    let on = if i == usize::MAX {
        0
    } else {
        LVIS_SELECTED.0 | LVIS_FOCUSED.0
    };
    let item = LVITEMW {
        stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        state: LIST_VIEW_ITEM_STATE_FLAGS(on),
        ..Default::default()
    };
    ui::send(
        c.list,
        LVM_SETITEMSTATE,
        i,
        std::ptr::from_ref(&item) as isize,
    );
    if i != usize::MAX {
        ui::send(c.list, LVM_ENSUREVISIBLE, i, 0);
    }
}

/// Re-runs the search, keeping the selected row when it's still there.
fn reload(select_first: bool) {
    let Some(c) = ctl() else {
        return;
    };
    let keep = selected(&c).and_then(row_at).map(|r| r.id);
    let query = ui::get_text(c.search);
    let (count, again) = data(|d| {
        d.rows = match &d.store {
            Some(st) => st.search(&query, LIMIT).unwrap_or_else(|e| {
                log::error!("history search: {e}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        let again = keep.and_then(|id| d.rows.iter().position(|r| r.id == id));
        (d.rows.len(), again)
    });
    select(&c, usize::MAX);
    ui::send(c.list, LVM_SETITEMCOUNT, count, 0);
    match again.or((select_first && count > 0).then_some(0)) {
        Some(i) => select(&c, i),
        None => update_detail(),
    }
    // SAFETY: repaint our own control.
    let _ = unsafe { InvalidateRect(Some(c.list), None, true) };
}

fn status_text(r: &Row) -> String {
    let base = match r.status.as_str() {
        store::OK => "",
        store::PROVISIONAL => "incomplete",
        store::FAILED => "failed",
        other => other,
    };
    match (base.is_empty(), r.audio_path.is_some()) {
        (_, false) => base.to_string(),
        (true, true) => "audio kept".into(),
        (false, true) => format!("{base} · audio kept"),
    }
}

fn update_detail() {
    let Some(c) = ctl() else {
        return;
    };
    let row = selected(&c).and_then(row_at);
    let (text, can_copy, can_retry) = match &row {
        Some(r) => {
            let mut t = r.text.replace("\r\n", "\n").replace('\n', "\r\n");
            if let Some(e) = &r.error {
                if !t.is_empty() {
                    t.push_str("\r\n\r\n");
                }
                t.push_str("Error: ");
                t.push_str(e);
            }
            if let Some(d) = r.duration_ms {
                t.push_str(&format!("\r\n\r\n{:.1} s", d as f64 / 1000.0));
                if let Some(m) = &r.model {
                    t.push_str(&format!(" · {m}"));
                }
            }
            (t, !r.text.is_empty(), r.audio_path.is_some())
        }
        None => (String::new(), false, false),
    };
    ui::set_text(c.detail, &text);
    // SAFETY: enabling our own controls.
    unsafe {
        let _ = EnableWindow(c.copy, can_copy);
        let _ = EnableWindow(c.retry, can_retry);
        let _ = EnableWindow(c.delete, row.is_some());
    }
}

fn cell_text(r: &Row, col: i32) -> String {
    match col {
        0 => ui::local_time(r.created_ms),
        1 => {
            let one: String = r
                .text
                .chars()
                .take(300)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            if one.is_empty() { "—".into() } else { one }
        }
        _ => status_text(r),
    }
}

fn get_dispinfo(lparam: LPARAM) {
    // SAFETY: LVN_GETDISPINFOW carries an NMLVDISPINFOW.
    let info = unsafe { &mut *(lparam.0 as *mut NMLVDISPINFOW) };
    if (info.item.mask & LVIF_TEXT).0 == 0 || info.item.cchTextMax <= 0 {
        return;
    }
    let max = info.item.cchTextMax as usize - 1;
    data(|d| {
        let Some(r) = d.rows.get(info.item.iItem as usize) else {
            return;
        };
        d.cell = cell_text(r, info.item.iSubItem)
            .encode_utf16()
            .take(max)
            .chain([0])
            .collect();
        // SAFETY: the list provides a buffer of cchTextMax chars; we write at most that.
        unsafe {
            std::ptr::copy_nonoverlapping(d.cell.as_ptr(), info.item.pszText.0, d.cell.len());
        }
    });
}

fn send_core(cmd: UiCmd) {
    super::ipc::send(Event::Ui(cmd));
}

fn act(id: usize) {
    let Some(c) = ctl() else {
        return;
    };
    let row = selected(&c).and_then(row_at);
    match id {
        ID_SETTINGS => super::settings_ui::show(),
        ID_COPY => {
            if let Some(r) = row.filter(|r| !r.text.is_empty()) {
                send_core(UiCmd::Copy(r.id));
            }
        }
        ID_RETRY => {
            if let Some(r) = row.filter(|r| r.audio_path.is_some()) {
                send_core(UiCmd::Retry(r.id));
            }
        }
        ID_DELETE => {
            if let Some(r) = row {
                let q = HSTRING::from(if r.audio_path.is_some() {
                    "Delete this dictation and its saved audio?"
                } else {
                    "Delete this dictation?"
                });
                // SAFETY: modal box owned by our window.
                let yes = unsafe {
                    MessageBoxW(Some(c.hwnd), &q, w!("gemdict"), MB_YESNO | MB_ICONWARNING)
                };
                if yes == IDYES {
                    send_core(UiCmd::Delete(r.id));
                }
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => layout(hwnd),
        WM_GETMINMAXINFO => {
            let dpi = ui::dpi(hwnd);
            // SAFETY: WM_GETMINMAXINFO carries a MINMAXINFO.
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = px(520, dpi);
            mmi.ptMinTrackSize.y = px(380, dpi);
        }
        WM_DPICHANGED => {
            // SAFETY: lparam is the suggested RECT.
            let r = unsafe { *(lparam.0 as *const RECT) };
            if let Some(mut c) = ctl() {
                let font = ui::font((wparam.0 & 0xFFFF) as u32);
                for h in [
                    c.search, c.list, c.detail, c.copy, c.retry, c.delete, c.settings,
                ] {
                    ui::send(h, WM_SETFONT, font.0 as usize, 1);
                }
                // SAFETY: no control uses the old font any more.
                let _ = unsafe { DeleteObject(std::mem::replace(&mut c.font, font).into()) };
                CTL.with(|x| x.set(Some(c)));
            }
            // SAFETY: resizing our own window to the suggested rect.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            let code = (wparam.0 >> 16) as u32;
            match id {
                ID_SEARCH if code == EN_CHANGE => {
                    // SAFETY: timer on our own window.
                    unsafe { SetTimer(Some(hwnd), SEARCH_TIMER, 200, None) };
                }
                x if x == IDCANCEL.0 as usize => {
                    // SAFETY: our own window.
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                }
                ID_COPY | ID_RETRY | ID_DELETE | ID_SETTINGS => act(id),
                _ => {}
            }
        }
        WM_TIMER if wparam.0 == SEARCH_TIMER => {
            // SAFETY: our own timer.
            let _ = unsafe { KillTimer(Some(hwnd), SEARCH_TIMER) };
            reload(true);
        }
        WM_APP_CHANGED => reload(false),
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY carries an NMHDR.
            let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
            if hdr.idFrom == ID_LIST {
                match hdr.code {
                    LVN_GETDISPINFOW => get_dispinfo(lparam),
                    LVN_ITEMCHANGED => update_detail(),
                    NM_DBLCLK => act(ID_COPY),
                    LVN_KEYDOWN => {
                        // SAFETY: LVN_KEYDOWN carries an NMLVKEYDOWN.
                        let k = unsafe { &*(lparam.0 as *const NMLVKEYDOWN) };
                        // SAFETY: plain key state query.
                        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                        if k.wVKey == VK_DELETE.0 {
                            act(ID_DELETE);
                        } else if ctrl && k.wVKey == u16::from(b'C') {
                            act(ID_COPY);
                        }
                    }
                    _ => {}
                }
            }
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => {
            HWND_.store(0, Ordering::Release);
            if let Some(c) = CTL.with(Cell::take) {
                // SAFETY: the controls are gone with the window.
                let _ = unsafe { DeleteObject(c.font.into()) };
            }
            // Drop the rows and the read connection while hidden.
            data(|d| *d = Data::default());
        }
        // SAFETY: default handling.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
