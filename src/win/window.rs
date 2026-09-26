//! Foreground window queries and focus restore for paste.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, HWND_MESSAGE,
    IsIconic, IsWindow, SW_RESTORE, SetForegroundWindow, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{PWSTR, w};

/// A window handle that can cross threads (HWNDs are process-global values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window(pub isize);

impl Window {
    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

pub fn foreground() -> Option<Window> {
    // SAFETY: plain query.
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid()).then_some(Window(h.0 as isize))
}

pub fn exists(w: Window) -> bool {
    // SAFETY: plain query; any value is accepted.
    unsafe { IsWindow(Some(w.hwnd())) }.as_bool()
}

fn pid_and_thread(w: Window) -> (u32, u32) {
    let mut pid = 0;
    // SAFETY: valid out-pointer.
    let tid = unsafe { GetWindowThreadProcessId(w.hwnd(), Some(&mut pid)) };
    (pid, tid)
}

pub fn is_own(w: Window) -> bool {
    pid_and_thread(w).0 == std::process::id()
}

pub fn class_name(w: Window) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buffer is valid for its length.
    let n = unsafe { GetClassNameW(w.hwnd(), &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn with_process<T>(pid: u32, f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    // SAFETY: handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let out = f(process);
    // SAFETY: we own the handle.
    let _ = unsafe { CloseHandle(process) };
    out
}

/// Lower-case exe file name of the window's process.
pub fn exe_name(w: Window) -> Option<String> {
    with_process(pid_and_thread(w).0, |process| {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: buffer and length are valid.
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit('\\').next().unwrap_or(&path).to_lowercase())
    })
}

fn integrity_of(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    // SAFETY: valid out-pointer; token closed below.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let mut buf = [0u8; 64];
    let mut len = 0;
    // SAFETY: buffer is valid; on success it holds a TOKEN_MANDATORY_LABEL whose SID points inside it.
    let level = unsafe {
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1)
    };
    Some(level)
}

/// True if the window's process runs at a higher integrity level than us (SendInput can't
/// reach it). If we can't even read its token, it's treated as elevated.
pub fn is_elevated(w: Window) -> bool {
    // SAFETY: pseudo-handle, no close needed.
    let Some(ours) = integrity_of(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    match with_process(pid_and_thread(w).0, integrity_of) {
        Some(theirs) => theirs > ours,
        None => true,
    }
}

/// Brings `w` to the foreground. SetForegroundWindow alone is blocked by the foreground
/// lock, so we attach to the target's and the current foreground's input queues first.
pub fn focus(w: Window) -> bool {
    if foreground() == Some(w) {
        return true;
    }
    // SAFETY: plain window calls; every attach is undone.
    unsafe {
        if IsIconic(w.hwnd()).as_bool() {
            let _ = ShowWindow(w.hwnd(), SW_RESTORE);
        }
        let me = GetCurrentThreadId();
        let target_tid = pid_and_thread(w).1;
        let fg_tid = foreground().map_or(0, |f| pid_and_thread(f).1);
        let attached_target = target_tid != 0
            && target_tid != me
            && AttachThreadInput(me, target_tid, true).as_bool();
        let attached_fg = fg_tid != 0
            && fg_tid != me
            && fg_tid != target_tid
            && AttachThreadInput(me, fg_tid, true).as_bool();
        let _ = SetForegroundWindow(w.hwnd());
        if attached_fg {
            let _ = AttachThreadInput(me, fg_tid, false);
        }
        if attached_target {
            let _ = AttachThreadInput(me, target_tid, false);
        }
    }
    foreground() == Some(w)
}

/// A message-only window owned by the calling thread (clipboard owner for paste).
pub fn message_window() -> windows::core::Result<HWND> {
    // SAFETY: creates a plain STATIC window under HWND_MESSAGE; it lives until the thread exits.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("dictap.paste"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
}
