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
use windows::core::w;

const CF_TEXT: u32 = 1;
const CF_OEMTEXT: u32 = 7;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_LOCALE: u32 = 16;

/// Formats we copy back byte-for-byte; all are plain HGLOBAL data.
fn saved_formats() -> [u32; 3] {
    // SAFETY: registering a well-known name just returns its id.
    let drop_effect = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    [CF_UNICODETEXT, CF_HDROP, drop_effect]
}

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

fn write_global(format: u32, bytes: &[u8]) -> bool {
    // SAFETY: we allocate, fill and hand the memory to the clipboard, freeing it only if that fails.
    unsafe {
        let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return false;
        };
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        if SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
            let _ = GlobalFree(Some(g));
            return false;
        }
        true
    }
}

pub fn save(owner: HWND) -> Option<Saved> {
    let _open = Open::new(owner)?;
    let wanted = saved_formats();
    let mut saved = Saved {
        items: Vec::new(),
        lossy: false,
    };
    let mut format = 0;
    loop {
        // SAFETY: clipboard is open.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if wanted.contains(&format) {
            if let Some(bytes) = read_global(format) {
                saved.items.push((format, bytes));
            }
        } else if !matches!(format, CF_TEXT | CF_OEMTEXT | CF_LOCALE) {
            saved.lossy = true; // synthesized text formats come back on their own
        }
    }
    Some(saved)
}

/// Replaces the clipboard with `text`, kept out of clipboard history and cloud sync.
/// Returns the new sequence number.
pub fn set_text(owner: HWND, text: &str) -> Option<u32> {
    let _open = Open::new(owner)?;
    // SAFETY: clipboard is open with an owner window.
    unsafe { EmptyClipboard() }.ok()?;
    let wide: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    if !write_global(CF_UNICODETEXT, &wide) {
        return None;
    }
    // SAFETY: registering well-known names.
    let (exclude, history, cloud) = unsafe {
        (
            RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
            RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
            RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")),
        )
    };
    write_global(exclude, &[]);
    write_global(history, &0u32.to_le_bytes());
    write_global(cloud, &0u32.to_le_bytes());
    drop(_open);
    Some(sequence())
}

pub fn restore(owner: HWND, saved: &Saved) -> bool {
    let Some(_open) = Open::new(owner) else {
        return false;
    };
    // SAFETY: clipboard is open with an owner window.
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    saved
        .items
        .iter()
        .all(|(format, bytes)| write_global(*format, bytes))
}

impl Saved {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
