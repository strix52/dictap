//! Clipboard access for paste: snapshot what we can, publish our text in the same open
//! transaction, and restore later only if nothing else has touched the clipboard since.

use std::sync::OnceLock;
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, SetLastError, WIN32_ERROR,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
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

/// Past this many bytes in total, the rest of a snapshot is dropped (and marked lossy).
const SAVE_MAX: usize = 64 << 20;
/// A clipboard never legitimately lists this many formats; stop a runaway enumeration.
const MAX_FORMATS: usize = 512;

/// Registered formats, looked up once so copy, paste and restore agree on the ids.
struct Formats {
    drop_effect: u32,
    html: u32,
    rtf: u32,
    png: u32,
    history: u32,
    cloud: u32,
    monitor: u32,
    viewer_ignore: u32,
}

impl Formats {
    /// Payload formats we copy back byte-for-byte; all are plain HGLOBAL data. Text, files,
    /// images and rich text survive a dictation; app-private formats don't.
    fn payload(&self) -> [u32; 7] {
        [
            CF_UNICODETEXT,
            CF_HDROP,
            CF_DIB,
            self.drop_effect,
            self.html,
            self.rtf,
            self.png,
        ]
    }

    /// The opt-out formats for clipboard history, cloud sync and clipboard managers.
    fn privacy(&self) -> [u32; 4] {
        [self.history, self.cloud, self.monitor, self.viewer_ignore]
    }
}

fn formats() -> &'static Formats {
    static FORMATS: OnceLock<Formats> = OnceLock::new();
    FORMATS.get_or_init(|| {
        let reg = |name: PCWSTR| {
            // SAFETY: registering a well-known name just returns its id (0 on failure).
            unsafe { RegisterClipboardFormatW(name) }
        };
        Formats {
            drop_effect: reg(w!("Preferred DropEffect")),
            html: reg(w!("HTML Format")),
            rtf: reg(w!("Rich Text Format")),
            png: reg(w!("PNG")),
            // Win+V history and cloud clipboard read the first two DWORDs.
            history: reg(w!("CanIncludeInClipboardHistory")),
            cloud: reg(w!("CanUploadToCloudClipboard")),
            // Clipboard managers (Ditto, 1Password, KeePass, PowerToys) look for these.
            monitor: reg(w!("ExcludeClipboardContentFromMonitorProcessing")),
            viewer_ignore: reg(w!("Clipboard Viewer Ignore")),
        }
    })
}

/// True if another `size` bytes still fit in the budget (checked: never wraps).
fn fits(used: usize, size: usize, max: usize) -> bool {
    used.checked_add(size).is_some_and(|total| total <= max)
}

/// Clipboard contents we could save. `lossy` is set when other formats were present
/// (images, rich text, app-private data) that restoring would drop.
pub struct Saved {
    items: Vec<(u32, Vec<u8>)>,
    pub lossy: bool,
}

/// Proof that we published `text` and what it replaced.
pub struct ClipboardLease {
    saved: Saved,
    owned_sequence: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// Another process held the clipboard for the whole retry window.
    Busy,
    /// Nothing was published (allocation or format failure); the clipboard is as it was.
    Failed,
    /// Publishing failed after the clipboard was emptied and putting it back failed too.
    RollbackFailed,
    /// Something else took the clipboard between our write and our ownership check.
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored,
    /// Restored, but some formats could not be saved and are gone.
    Lossy,
    /// Someone copied something since; theirs was left alone.
    Changed,
    /// The clipboard stayed busy; whatever is there was left alone.
    Busy,
    Failed,
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

/// True if the window that last emptied the clipboard is `owner`.
fn owner_is(owner: HWND) -> bool {
    // SAFETY: plain query.
    unsafe { GetClipboardOwner() }.is_ok_and(|h| h == owner)
}

/// True while the clipboard still holds exactly what `lease` published.
pub fn still_owned(lease: &ClipboardLease) -> bool {
    sequence() == lease.owned_sequence
}

#[derive(Debug, PartialEq, Eq)]
enum ReadFormatError {
    Unavailable,
    BudgetExceeded,
}

/// Unlocks a locked global on drop.
struct Locked {
    handle: HGLOBAL,
    ptr: *const u8,
}

impl Locked {
    fn new(handle: HGLOBAL) -> Option<Locked> {
        // SAFETY: the handle is a live global (clipboard-owned or ours).
        let ptr = unsafe { GlobalLock(handle) }.cast::<u8>().cast_const();
        (!ptr.is_null()).then_some(Locked { handle, ptr })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: balances the successful GlobalLock in `new`.
        let _ = unsafe { GlobalUnlock(self.handle) };
    }
}

/// Copies one format out of the open clipboard, refusing before allocation if it is larger
/// than `remaining` bytes.
fn read_global(format: u32, remaining: usize) -> Result<Vec<u8>, ReadFormatError> {
    // SAFETY: the clipboard is open on this thread; the handle stays owned by the clipboard.
    let handle = unsafe { GetClipboardData(format) }.map_err(|_| ReadFormatError::Unavailable)?;
    let g = HGLOBAL(handle.0);
    // SAFETY: plain calls; GlobalSize returns 0 both for "empty" and for failure, so the last
    // error is cleared first and checked after.
    let size = unsafe {
        SetLastError(WIN32_ERROR(0));
        GlobalSize(g)
    };
    if size == 0 {
        // SAFETY: plain query.
        return if unsafe { GetLastError() } == ERROR_SUCCESS {
            Ok(Vec::new())
        } else {
            Err(ReadFormatError::Unavailable)
        };
    }
    if size > remaining {
        return Err(ReadFormatError::BudgetExceeded);
    }
    let lock = Locked::new(g).ok_or(ReadFormatError::Unavailable)?;
    // SAFETY: `lock` holds the global locked; GlobalSize reported `size` readable bytes.
    Ok(unsafe { std::slice::from_raw_parts(lock.ptr, size) }.to_vec())
}

/// Copies `bytes` into a new movable global for the clipboard.
fn alloc(bytes: &[u8]) -> Option<HGLOBAL> {
    // SAFETY: we allocate and fill the memory; the caller hands it over or frees it.
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let p = GlobalLock(g).cast::<u8>();
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        Some(g)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReplaceError {
    /// Failed before touching the clipboard.
    Untouched,
    /// The clipboard was emptied and is now incomplete.
    Partial,
}

/// Replaces the (open) clipboard's contents with `items`, in order, stopping at the first
/// failure. Every global is allocated before anything is emptied, so an allocation failure
/// leaves the clipboard as it was. Callers put the transcript last, so it is only published
/// after every privacy format has been.
fn replace(items: &[(u32, &[u8])]) -> Result<(), ReplaceError> {
    let mut globals = Vec::with_capacity(items.len());
    for (format, bytes) in items {
        match alloc(bytes) {
            Some(g) if *format != 0 => globals.push((*format, g)),
            other => {
                if let Some(g) = other {
                    // SAFETY: allocated here and never handed to the clipboard.
                    let _ = unsafe { GlobalFree(Some(g)) };
                }
                for (_, g) in globals {
                    // SAFETY: as above.
                    let _ = unsafe { GlobalFree(Some(g)) };
                }
                return Err(ReplaceError::Untouched);
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
            return Err(ReplaceError::Untouched);
        }
        let mut failed = false;
        for (format, g) in globals {
            if failed || SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
                if !failed {
                    failed = true;
                }
                let _ = GlobalFree(Some(g));
            }
        }
        if failed {
            Err(ReplaceError::Partial)
        } else {
            Ok(())
        }
    }
}

/// Snapshots the supported formats of the open clipboard, including the original privacy
/// formats' raw bytes (their absence is preserved too: nothing is invented).
fn snapshot() -> Saved {
    let f = formats();
    let wanted: Vec<u32> = f
        .payload()
        .into_iter()
        .chain(f.privacy())
        .filter(|&id| id != 0)
        .collect();
    let mut saved = Saved {
        items: Vec::new(),
        lossy: false,
    };
    let (mut format, mut used, mut seen) = (0u32, 0usize, 0usize);
    loop {
        // SAFETY: the clipboard is open. The last error is cleared first because a zero
        // return means both "no more formats" and "failed".
        format = unsafe {
            SetLastError(WIN32_ERROR(0));
            EnumClipboardFormats(format)
        };
        if format == 0 {
            // SAFETY: plain query.
            if unsafe { GetLastError() } != ERROR_SUCCESS {
                saved.lossy = true;
            }
            break;
        }
        seen += 1;
        if seen > MAX_FORMATS {
            saved.lossy = true;
            break;
        }
        if wanted.contains(&format) {
            match read_global(format, SAVE_MAX - used) {
                Ok(bytes) if fits(used, bytes.len(), SAVE_MAX) => {
                    used += bytes.len();
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
    saved
}

/// The items for publishing `text`: privacy opt-outs first (when `private`), text last.
fn transcript_items(text: &str, private: bool) -> Result<Vec<(u32, Vec<u8>)>, ClipboardError> {
    let wide: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut items = Vec::with_capacity(5);
    if private {
        let f = formats();
        let no = 0u32.to_le_bytes().to_vec();
        for id in f.privacy() {
            if id == 0 {
                // Never publish an unprotected transcript when protection was requested.
                return Err(ClipboardError::Failed);
            }
            items.push((id, no.clone()));
        }
    }
    items.push((CF_UNICODETEXT, wide));
    Ok(items)
}

fn refs(items: &[(u32, Vec<u8>)]) -> Vec<(u32, &[u8])> {
    items.iter().map(|(f, b)| (*f, b.as_slice())).collect()
}

/// Snapshots the clipboard and publishes `text` (private: kept out of history, cloud sync and
/// managers) in one open transaction, so a copy made by the user cannot slip in between.
pub fn begin_paste(owner: HWND, text: &str) -> Result<ClipboardLease, ClipboardError> {
    let items = transcript_items(text, true)?;
    let open = Open::new(owner).ok_or(ClipboardError::Busy)?;
    let saved = snapshot();
    match replace(&refs(&items)) {
        Ok(()) => {}
        Err(ReplaceError::Untouched) => return Err(ClipboardError::Failed),
        Err(ReplaceError::Partial) => {
            return Err(if replace(&refs_saved(&saved)).is_ok() {
                ClipboardError::Failed
            } else {
                ClipboardError::RollbackFailed
            });
        }
    }
    drop(open);
    // The sequence is read after the close so it is the value a later check will see. If the
    // clipboard changed hands in between, its owner is no longer our window.
    let owned_sequence = sequence();
    if !owner_is(owner) {
        return Err(ClipboardError::Changed);
    }
    Ok(ClipboardLease {
        saved,
        owned_sequence,
    })
}

fn refs_saved(saved: &Saved) -> Vec<(u32, &[u8])> {
    refs(&saved.items)
}

/// Puts the clipboard back as `lease` found it, but only if it still holds what we published.
pub fn restore_if_unchanged(owner: HWND, lease: &ClipboardLease) -> RestoreOutcome {
    let Some(_open) = Open::new(owner) else {
        return RestoreOutcome::Busy;
    };
    // Compare under the open clipboard: nobody can change it between this and the replace.
    if sequence() != lease.owned_sequence || !owner_is(owner) {
        return RestoreOutcome::Changed;
    }
    match replace(&refs_saved(&lease.saved)) {
        Ok(()) if lease.saved.lossy => RestoreOutcome::Lossy,
        Ok(()) => RestoreOutcome::Restored,
        Err(_) => RestoreOutcome::Failed,
    }
}

/// Replaces the clipboard with `text` and keeps no way back (copy fallbacks and the History
/// Copy button). `private` keeps it out of clipboard history, cloud sync and managers.
pub fn copy_text(owner: HWND, text: &str, private: bool) -> Result<(), ClipboardError> {
    let items = transcript_items(text, private)?;
    let _open = Open::new(owner).ok_or(ClipboardError::Busy)?;
    replace(&refs(&items)).map_err(|_| ClipboardError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_checks_never_wrap() {
        assert!(fits(0, 10, 10));
        assert!(!fits(1, 10, 10));
        assert!(!fits(usize::MAX, 1, SAVE_MAX));
        assert!(!fits(SAVE_MAX, usize::MAX, SAVE_MAX));
    }

    #[test]
    fn transcript_items_put_text_last() {
        // Private items need registered ids; with them the transcript is the final item.
        let items = transcript_items("hi", false).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, CF_UNICODETEXT);
        assert_eq!(items[0].1, [b'h', 0, b'i', 0, 0, 0]);
    }
    #[test]
    #[ignore = "temporarily replaces the real clipboard with synthetic data"]
    fn synthetic_clipboard_roundtrip_real() {
        let owner = crate::win::window::message_window().unwrap();
        struct Restore {
            owner: HWND,
            saved: Saved,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(_open) = Open::new(self.owner) {
                    let _ = replace(&refs_saved(&self.saved));
                }
            }
        }
        let original = {
            let _open = Open::new(owner).unwrap();
            snapshot()
        };
        assert!(
            !original.lossy,
            "Native smoke requires a restorable original clipboard; nothing changed"
        );
        let _restore = Restore {
            owner,
            saved: original,
        };
        let mut before = transcript_items("dictap synthetic original", true).unwrap();
        for (_, bytes) in before.iter_mut().take(4) {
            *bytes = 1u32.to_le_bytes().to_vec();
        }
        {
            let _open = Open::new(owner).unwrap();
            replace(&refs(&before)).unwrap();
        }
        let lease = begin_paste(owner, "dictap synthetic replacement").unwrap();
        assert!(still_owned(&lease));
        assert_eq!(
            restore_if_unchanged(owner, &lease),
            RestoreOutcome::Restored
        );
        {
            let _open = Open::new(owner).unwrap();
            for (format, bytes) in &before {
                assert_eq!(&read_global(*format, SAVE_MAX).unwrap(), bytes);
            }
        }
        let lease = begin_paste(owner, "dictap synthetic second").unwrap();
        copy_text(owner, "dictap synthetic competing copy", true).unwrap();
        assert_eq!(restore_if_unchanged(owner, &lease), RestoreOutcome::Changed);
    }
}
