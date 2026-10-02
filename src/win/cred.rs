//! Windows Credential Manager (generic credentials).

use windows::Win32::Foundation::{ERROR_INVALID_DATA, ERROR_NOT_FOUND, WIN32_ERROR};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::{HSTRING, PWSTR};

fn is_not_found(e: &windows::core::Error) -> bool {
    e.code() == WIN32_ERROR(ERROR_NOT_FOUND.0).to_hresult()
}

/// Upper bound for a stored secret. API keys are tens of bytes; anything near this is corrupt.
const BLOB_MAX: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobError {
    /// A nonempty blob with a null pointer.
    Null,
    TooLarge,
}

/// Copies `len` bytes from an API-provided blob. An empty blob never touches the pointer.
///
/// # Safety
/// When `len > 0` and `blob` is non-null, `blob` must be valid for reads of `len` bytes.
unsafe fn copy_blob(blob: *const u8, len: usize) -> Result<Vec<u8>, BlobError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if blob.is_null() {
        return Err(BlobError::Null);
    }
    if len > BLOB_MAX {
        return Err(BlobError::TooLarge);
    }
    // SAFETY: non-null, bounded, and valid for `len` bytes by the caller's contract.
    Ok(unsafe { std::slice::from_raw_parts(blob, len) }.to_vec())
}

/// Owns a credential returned by `CredReadW`: wipes the secret and frees it on every path.
struct Credential(*mut CREDENTIALW);

impl Drop for Credential {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful CredReadW and is freed exactly once here.
        unsafe {
            let c = &*self.0;
            if !c.CredentialBlob.is_null() && c.CredentialBlobSize > 0 {
                // Only wipe a blob we were willing to read; the size is API-provided.
                let n = (c.CredentialBlobSize as usize).min(BLOB_MAX);
                std::ptr::write_bytes(c.CredentialBlob, 0, n);
            }
            CredFree(self.0 as *const _);
        }
    }
}

/// Reads a generic credential's secret bytes. `Ok(None)` when it doesn't exist. An empty or
/// malformed blob is an error rather than a usable (empty) secret.
pub fn read(target: &str) -> windows::core::Result<Option<Vec<u8>>> {
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `cred` is a valid out-pointer; on success it points to a CREDENTIALW we free below.
    match unsafe { CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut cred) } {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    }
    if cred.is_null() {
        return Err(invalid_data());
    }
    let guard = Credential(cred);
    // SAFETY: CredReadW succeeded, so `cred` is valid and `guard` keeps it alive.
    let c = unsafe { &*guard.0 };
    // SAFETY: the API promises `CredentialBlobSize` readable bytes at `CredentialBlob`.
    let bytes = unsafe { copy_blob(c.CredentialBlob, c.CredentialBlobSize as usize) }
        .map_err(|_| invalid_data())?;
    Ok(Some(bytes))
}

fn invalid_data() -> windows::core::Error {
    windows::core::Error::from(ERROR_INVALID_DATA.to_hresult())
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
#[cfg_attr(not(test), allow(dead_code))]
pub fn delete(target: &str) -> windows::core::Result<()> {
    // SAFETY: plain call with a valid wide string.
    match unsafe { CredDeleteW(&HSTRING::from(target), CRED_TYPE_GENERIC, None) } {
        Err(e) if !is_not_found(&e) => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_copy_guards() {
        // SAFETY: each call's pointer/length pair is valid or the call rejects it first.
        unsafe {
            assert_eq!(copy_blob(std::ptr::null(), 0), Ok(Vec::new()));
            assert_eq!(copy_blob(std::ptr::null(), 4), Err(BlobError::Null));
            let data = [1u8, 2, 3];
            assert_eq!(copy_blob(data.as_ptr(), 3), Ok(vec![1, 2, 3]));
            assert_eq!(
                copy_blob(data.as_ptr(), BLOB_MAX + 1),
                Err(BlobError::TooLarge)
            );
        }
    }

    /// Writes and deletes a real Credential Manager entry for this user, so it never runs by
    /// default. Run it by name on a machine where that is acceptable.
    #[test]
    #[ignore = "mutates Windows Credential Manager"]
    fn roundtrip() {
        let target = format!("dictap/test-{}", std::process::id());
        super::write(&target, "test", b"s3cret").unwrap();
        assert_eq!(
            super::read(&target).unwrap().as_deref(),
            Some(&b"s3cret"[..])
        );
        super::delete(&target).unwrap();
        assert_eq!(super::read(&target).unwrap(), None);
        super::delete(&target).unwrap();
    }
}
