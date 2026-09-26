//! Clipboard access for paste: save what we can, set our text, restore later.

use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::core::{PCWSTR, w};

const CF_TEXT: u32 = 1;
const CF_OEMTEXT: u32 = 7;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIB: u32 = 8;
const CF_BITMAP: u32 = 2;
const CF_DIBV5: u32 = 17;
const CF_LOCALE: u32 = 16;

/// Formats we copy back byte-for-byte; all are plain HGLOBAL data. Text, files, images
/// and rich text survive a dictation; app-private formats (Office's native ones) don't.
fn saved_formats() -> [u32; 7] {
    let reg = |name: PCWSTR| {
        // SAFETY: registering a well-known name just returns its id.
        unsafe { RegisterClipboardFormatW(name) }
    };
    [
        CF_UNICODETEXT,
        CF_HDROP,
        CF_DIB,
        reg(w!("Preferred DropEffect")),
        reg(w!("HTML Format")),
        reg(w!("Rich Text Format")),
        reg(w!("PNG")),
    ]
}

/// Past this, the rest of a save is dropped (and marked lossy).
const SAVE_MAX: usize = 64 << 20;

/// Clipboard contents we could save. `lossy` is set when other formats were present
/// (images, rich text) that restoring would drop.
pub struct Saved {
    items: Vec<(u32, Vec<u8>)>,
    pub lossy: bool,
}

/// Open clipboard; closed on drop.
struct Open;

impl Open {
    fn new(owner: HWND) -> Option<Open> {
        for _ in 0..10 {
            // SAFETY: plain call; paired with CloseClipboard in Drop.
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Some(Open);
            }
            sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: we opened it.
        let _ = unsafe { CloseClipboard() };
    }
}

pub fn sequence() -> u32 {
    // SAFETY: plain query.
    unsafe { GetClipboardSequenceNumber() }
}

fn read_global(format: u32) -> Option<Vec<u8>> {
    // SAFETY: clipboard is open; the handle stays owned by the clipboard, we only lock/copy it.
    unsafe {
        let h = GetClipboardData(format).ok()?;
        let g = HGLOBAL(h.0);
        let size = GlobalSize(g);
        let p = GlobalLock(g) as *const u8;
        if p.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(p, size).to_vec();
        let _ = GlobalUnlock(g);
        Some(bytes)
    }
}

/// Copies `bytes` into a new movable global for the clipboard.
fn alloc(bytes: &[u8]) -> Option<HGLOBAL> {
    // SAFETY: we allocate and fill the memory; the caller hands it over or frees it.
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        Some(g)
    }
}

/// Replaces the (open) clipboard's contents with `items`. Every global is allocated before
/// anything is emptied, so a failed allocation leaves the clipboard as it was.
fn replace(items: &[(u32, &[u8])]) -> bool {
    let mut globals = Vec::with_capacity(items.len());
    for (format, bytes) in items {
        match alloc(bytes) {
            Some(g) => globals.push((*format, g)),
            None => {
                for (_, g) in globals {
                    // SAFETY: still ours; never handed to the clipboard.
                    let _ = unsafe { GlobalFree(Some(g)) };
                }
                return false;
            }
        }
    }
    // SAFETY: the clipboard is open with an owner window. Each global belongs to the
    // clipboard once SetClipboardData succeeds, and is freed here if it doesn't.
    unsafe {
        if EmptyClipboard().is_err() {
            for (_, g) in globals {
                let _ = GlobalFree(Some(g));
            }
            return false;
        }
        let mut ok = true;
        for (format, g) in globals {
            if SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
                let _ = GlobalFree(Some(g));
                ok = false;
            }
        }
        ok
    }
}

pub fn save(owner: HWND) -> Option<Saved> {
    let _open = Open::new(owner)?;
    let wanted = saved_formats();
    let mut saved = Saved {
        items: Vec::new(),
        lossy: false,
    };
    let (mut format, mut size) = (0, 0);
    loop {
        // SAFETY: clipboard is open.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if wanted.contains(&format) {
            match read_global(format) {
                Some(bytes) if size + bytes.len() <= SAVE_MAX => {
                    size += bytes.len();
                    saved.items.push((format, bytes));
                }
                _ => saved.lossy = true,
            }
        } else if !matches!(
            format,
            CF_TEXT | CF_OEMTEXT | CF_LOCALE | CF_BITMAP | CF_DIBV5
        ) {
            saved.lossy = true; // synthesized formats come back on their own
        }
    }
    Some(saved)
}

/// Replaces the clipboard with `text`. `private` keeps it out of Windows clipboard history,
/// cloud sync and clipboard managers that honour the opt-out formats.
/// Returns the new sequence number.
pub fn set_text(owner: HWND, text: &str, private: bool) -> Option<u32> {
    let wide: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    let reg = |name: PCWSTR| {
        // SAFETY: registering a well-known name just returns its id.
        unsafe { RegisterClipboardFormatW(name) }
    };
    let no = 0u32.to_le_bytes();
    let mut items: Vec<(u32, &[u8])> = Vec::with_capacity(5);
    if private {
        items.extend([
            // Win+V history and cloud clipboard read these two DWORDs.
            (reg(w!("CanIncludeInClipboardHistory")), &no[..]),
            (reg(w!("CanUploadToCloudClipboard")), &no[..]),
            // Clipboard managers (Ditto, 1Password, KeePass, PowerToys) look for these.
            (
                reg(w!("ExcludeClipboardContentFromMonitorProcessing")),
                &no[..],
            ),
            (reg(w!("Clipboard Viewer Ignore")), &no[..]),
        ]);
    }
    items.push((CF_UNICODETEXT, &wide));
    let open = Open::new(owner)?;
    let ok = replace(&items);
    drop(open);
    ok.then(sequence)
}

/// Puts back what `save` kept; an empty save empties the clipboard.
pub fn restore(owner: HWND, saved: &Saved) -> bool {
    let Some(_open) = Open::new(owner) else {
        return false;
    };
    let items: Vec<(u32, &[u8])> = saved
        .items
        .iter()
        .map(|(f, b)| (*f, b.as_slice()))
        .collect();
    replace(&items)
}
