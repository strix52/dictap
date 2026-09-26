//! Start with Windows via the HKCU `Run` key.

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{PCWSTR, w};

const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const NAME: PCWSTR = w!("gemdict");

fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: valid out-pointer; the key is closed by the caller.
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN, None, access, &mut key) }
        .ok()
        .ok()?;
    Some(key)
}

pub fn enabled() -> bool {
    let Some(key) = open(KEY_QUERY_VALUE) else {
        return false;
    };
    // SAFETY: size-only query on an open key, then closed.
    unsafe {
        let found = RegQueryValueExW(key, NAME, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        found
    }
}

pub fn set(on: bool) -> Result<(), String> {
    let key = open(KEY_SET_VALUE).ok_or("Couldn't open the Run key")?;
    // SAFETY: open key; the data buffer is valid for the call; closed after.
    let r = unsafe {
        let r = if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let value = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2);
            RegSetValueExW(key, NAME, None, REG_SZ, Some(bytes))
        } else {
            match RegDeleteValueW(key, NAME) {
                e if e == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND => Default::default(),
                e => e,
            }
        };
        let _ = RegCloseKey(key);
        r
    };
    r.ok().map_err(|e| format!("autostart: {e}"))
}
