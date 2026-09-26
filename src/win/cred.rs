//! Windows Credential Manager (generic credentials).

use windows::Win32::Foundation::{ERROR_NOT_FOUND, WIN32_ERROR};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{HSTRING, PWSTR};

fn is_not_found(e: &windows::core::Error) -> bool {
    e.code() == WIN32_ERROR(ERROR_NOT_FOUND.0).to_hresult()
}

/// Reads a generic credential's secret bytes. `Ok(None)` when it doesn't exist.
pub fn read(target: &str) -> windows::core::Result<Option<Vec<u8>>> {
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `cred` is a valid out-pointer; on success it points to a CREDENTIALW we free below.
    match unsafe { CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut cred) } {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    }
    // SAFETY: CredReadW succeeded, so `cred` is valid and its blob has `CredentialBlobSize` bytes.
    let bytes = unsafe {
        let c = &*cred;
        let blob = if c.CredentialBlob.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        };
        std::ptr::write_bytes(c.CredentialBlob, 0, c.CredentialBlobSize as usize);
        CredFree(cred as *const _);
        blob
    };
    Ok(Some(bytes))
}

pub fn write(target: &str, user: &str, secret: &[u8]) -> windows::core::Result<()> {
    let mut target_w: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let mut user_w: Vec<u16> = user.encode_utf16().chain([0]).collect();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        CredentialBlobSize: secret.len() as u32,
        CredentialBlob: secret.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user_w.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: all pointers in `cred` point to live buffers for the duration of the call.
    unsafe { CredWriteW(&cred, 0) }
}

/// Deletes a generic credential; missing is fine.
pub fn delete(target: &str) -> windows::core::Result<()> {
    // SAFETY: plain call with a valid wide string.
    match unsafe { CredDeleteW(&HSTRING::from(target), CRED_TYPE_GENERIC, None) } {
        Err(e) if !is_not_found(&e) => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let target = format!("gemdict/test-{}", std::process::id());
        super::write(&target, "test", b"s3cret").unwrap();
        assert_eq!(super::read(&target).unwrap().as_deref(), Some(&b"s3cret"[..]));
        super::delete(&target).unwrap();
        assert_eq!(super::read(&target).unwrap(), None);
        super::delete(&target).unwrap();
    }
}
