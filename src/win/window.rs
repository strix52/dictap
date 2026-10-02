//! Foreground window queries and focus restore for paste.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
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

pub fn pid_and_thread(w: Window) -> (u32, u32) {
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

/// Owns a process or token handle and closes it on every path.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the wrapper is the sole owner of a handle that opened successfully.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn with_process<T>(pid: u32, f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    // SAFETY: the handle is owned (and closed) by the guard below.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    f(process.0)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IntegrityLevel(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityError {
    OpenToken,
    Query,
    /// The returned structure did not describe a complete, in-bounds SID.
    Malformed,
}

/// Upper bound for the label we are willing to allocate (a mandatory label holds one SID of
/// at most 68 bytes, so this is generous).
const TOKEN_LABEL_MAX: usize = 1024;

/// Reads the integrity RID out of a `TOKEN_MANDATORY_LABEL` blob. `buf` is the filled part of
/// the query buffer. The SID pointer inside the label is only followed after proving that the
/// whole SID lies inside `buf`; nothing outside the slice is ever read, and nothing is
/// dereferenced through a typed reference, so alignment of `buf` does not matter here.
fn parse_integrity_label(buf: &[u8]) -> Result<IntegrityLevel, IntegrityError> {
    use std::mem::size_of;
    const SID_HEADER: usize = 8; // revision, subauthority count, 6-byte identifier authority
    if buf.len() < size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(IntegrityError::Malformed);
    }
    let base = buf.as_ptr() as usize;
    let end = base + buf.len();
    // The first field of the label is the SID pointer.
    let mut raw = [0u8; size_of::<usize>()];
    raw.copy_from_slice(&buf[..size_of::<usize>()]);
    let sid = usize::from_ne_bytes(raw);
    let first = base + size_of::<TOKEN_MANDATORY_LABEL>();
    let header_end = sid
        .checked_add(SID_HEADER)
        .ok_or(IntegrityError::Malformed)?;
    if sid < first || header_end > end {
        return Err(IntegrityError::Malformed);
    }
    let off = sid - base;
    let count = usize::from(buf[off + 1]);
    if count == 0 {
        return Err(IntegrityError::Malformed);
    }
    let sid_end = off + SID_HEADER + 4 * count;
    if sid_end > buf.len() {
        return Err(IntegrityError::Malformed);
    }
    let mut rid = [0u8; 4];
    rid.copy_from_slice(&buf[sid_end - 4..sid_end]);
    Ok(IntegrityLevel(u32::from_le_bytes(rid)))
}

fn integrity_of(process: HANDLE) -> Result<IntegrityLevel, IntegrityError> {
    let mut token = HANDLE::default();
    // SAFETY: valid out-pointer; on success the guard below owns and closes the token.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .map_err(|_| IntegrityError::OpenToken)?;
    let token = OwnedHandle(token);
    // First call: learn the size. It must fail with a length (a null buffer cannot succeed).
    let mut needed = 0u32;
    // SAFETY: null buffer with zero length is the documented size query; `needed` is valid.
    let _ = unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut needed) };
    let needed = needed as usize;
    if needed < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() || needed > TOKEN_LABEL_MAX {
        return Err(IntegrityError::Query);
    }
    // u64 storage gives the 8-byte alignment the label's pointer field wants.
    let mut storage = vec![0u64; needed.div_ceil(8)];
    let capacity = storage.len() * 8;
    let mut written = 0u32;
    // SAFETY: `storage` is valid for `capacity` bytes and outlives the call and the parse.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(storage.as_mut_ptr().cast()),
            capacity as u32,
            &mut written,
        )
    }
    .map_err(|_| IntegrityError::Query)?;
    let written = written as usize;
    if written > capacity {
        return Err(IntegrityError::Malformed);
    }
    // SAFETY: `written <= capacity` bytes of the u64 buffer were initialised by the call.
    let bytes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), written) };
    parse_integrity_label(bytes)
}

/// How the window's process compares with ours for synthetic input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Same or lower integrity: SendInput can reach it.
    Reachable,
    /// Higher integrity: UIPI blocks our input.
    Blocked,
    /// Either token could not be read. Callers treat this like `Blocked` and copy instead.
    Unknown,
}

pub fn reach(w: Window) -> Reach {
    // SAFETY: pseudo-handle, no close needed.
    let Ok(ours) = integrity_of(unsafe { GetCurrentProcess() }) else {
        return Reach::Unknown;
    };
    match with_process(pid_and_thread(w).0, |p| integrity_of(p).ok()) {
        Some(theirs) if theirs > ours => Reach::Blocked,
        Some(_) => Reach::Reachable,
        None => Reach::Unknown,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic label: SID_AND_ATTRIBUTES (pointer, attributes) followed by the SID bytes.
    fn label(count: u8, subs: &[u32], pointer: Option<usize>) -> Vec<u64> {
        let mut bytes = vec![0u8; 16];
        bytes.extend([1, count, 0, 0, 0, 0, 0, 16]);
        for s in subs {
            bytes.extend(s.to_le_bytes());
        }
        let mut words = vec![0u64; bytes.len().div_ceil(8)];
        // SAFETY: the u64 buffer is at least `bytes.len()` bytes long.
        let dst = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8)
        };
        dst[..bytes.len()].copy_from_slice(&bytes);
        let base = words.as_ptr() as usize;
        let sid = pointer.unwrap_or(base + 16);
        dst[..8].copy_from_slice(&sid.to_ne_bytes());
        words
    }

    fn view(words: &[u64], len: usize) -> &[u8] {
        // SAFETY: `len` never exceeds the buffer in these tests.
        unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len) }
    }

    #[test]
    fn integrity_label_parses_last_subauthority() {
        let w = label(1, &[0x2000], None);
        assert_eq!(
            parse_integrity_label(view(&w, 28)),
            Ok(IntegrityLevel(0x2000))
        );
        let w = label(2, &[7, 0x3000], None);
        assert_eq!(
            parse_integrity_label(view(&w, 32)),
            Ok(IntegrityLevel(0x3000))
        );
    }

    #[test]
    fn integrity_label_rejects_bad_structures() {
        let w = label(0, &[], None);
        assert_eq!(
            parse_integrity_label(view(&w, 24)),
            Err(IntegrityError::Malformed)
        );
        // Claims two subauthorities but only one is inside the returned bytes.
        let w = label(2, &[0x2000], None);
        assert_eq!(
            parse_integrity_label(view(&w, 28)),
            Err(IntegrityError::Malformed)
        );
        // SID pointer outside the buffer, and one pointing into the header.
        let w = label(1, &[0x2000], Some(8));
        assert_eq!(
            parse_integrity_label(view(&w, 28)),
            Err(IntegrityError::Malformed)
        );
        let w = label(1, &[0x2000], Some(usize::MAX - 2));
        assert_eq!(
            parse_integrity_label(view(&w, 28)),
            Err(IntegrityError::Malformed)
        );
        // Too short to hold the label header.
        let w = label(1, &[0x2000], None);
        assert_eq!(
            parse_integrity_label(view(&w, 8)),
            Err(IntegrityError::Malformed)
        );
    }

    #[test]
    fn own_integrity_is_readable() {
        // Reads only our own process token.
        // SAFETY: pseudo-handle.
        let level = integrity_of(unsafe { GetCurrentProcess() }).expect("own token");
        assert!(level.0 >= 0x1000);
    }
}
