//! AES-256-GCM decrypt through Windows CNG (BCrypt), for OpenWhispr's secure-keys files.

use windows::Win32::Foundation::STATUS_AUTH_TAG_MISMATCH;
use windows::Win32::Security::Cryptography::{
    BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_CHAIN_MODE_GCM, BCRYPT_CHAINING_MODE,
    BCRYPT_FLAGS, BCRYPT_KEY_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    BCryptCloseAlgorithmProvider, BCryptDecrypt, BCryptDestroyKey, BCryptGenerateSymmetricKey,
    BCryptOpenAlgorithmProvider, BCryptSetProperty,
};
use windows::core::PCWSTR;

/// Closes the algorithm provider and key on drop.
struct Handles {
    alg: BCRYPT_ALG_HANDLE,
    key: BCRYPT_KEY_HANDLE,
}

impl Drop for Handles {
    fn drop(&mut self) {
        // SAFETY: each handle is either default (skipped) or one we opened.
        unsafe {
            if !self.key.is_invalid() {
                let _ = BCryptDestroyKey(self.key);
            }
            if !self.alg.is_invalid() {
                let _ = BCryptCloseAlgorithmProvider(self.alg, 0);
            }
        }
    }
}

/// Decrypts `ct` with a 32-byte key, 12-byte IV and 16-byte tag. Errors carry no key material.
pub fn decrypt(
    key: &[u8; 32],
    iv: &[u8; 12],
    tag: &[u8; 16],
    ct: &[u8],
) -> Result<Vec<u8>, String> {
    let mut h = Handles {
        alg: BCRYPT_ALG_HANDLE::default(),
        key: BCRYPT_KEY_HANDLE::default(),
    };
    // SAFETY: out-pointers are valid; the chaining-mode buffer includes the UTF-16 NUL,
    // which the property requires; the mode info points at buffers that outlive the call.
    unsafe {
        BCryptOpenAlgorithmProvider(
            &mut h.alg,
            BCRYPT_AES_ALGORITHM,
            PCWSTR::null(),
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .ok()
        .map_err(|e| format!("AES provider: {e}"))?;
        let mode = std::slice::from_raw_parts(
            BCRYPT_CHAIN_MODE_GCM.as_ptr().cast::<u8>(),
            (BCRYPT_CHAIN_MODE_GCM.len() + 1) * 2,
        );
        BCryptSetProperty(h.alg.into(), BCRYPT_CHAINING_MODE, mode, 0)
            .ok()
            .map_err(|e| format!("GCM mode: {e}"))?;
        BCryptGenerateSymmetricKey(h.alg, &mut h.key, None, key, 0)
            .ok()
            .map_err(|e| format!("AES key: {e}"))?;

        let info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: iv.as_ptr().cast_mut(),
            cbNonce: iv.len() as u32,
            pbTag: tag.as_ptr().cast_mut(),
            cbTag: tag.len() as u32,
            ..Default::default()
        };
        let mut pt = vec![0u8; ct.len()];
        let mut n = 0u32;
        let status = BCryptDecrypt(
            h.key,
            Some(ct),
            Some(std::ptr::from_ref(&info).cast()),
            None,
            Some(&mut pt),
            &mut n,
            BCRYPT_FLAGS(0),
        );
        if status == STATUS_AUTH_TAG_MISMATCH {
            return Err("wrong key or damaged file".into());
        }
        status.ok().map_err(|e| format!("decrypt: {e}"))?;
        pt.truncate(n as usize);
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nist_case_14_and_tamper() {
        // NIST GCM test case 14: zero key, zero IV, one zero block.
        let ct = [
            0xce, 0xa7, 0x40, 0x3d, 0x4d, 0x60, 0x6b, 0x6e, 0x07, 0x4e, 0xc5, 0xd3, 0xba, 0xf3,
            0x9d, 0x18,
        ];
        let mut tag = [
            0xd0, 0xd1, 0xc8, 0xa7, 0x99, 0x99, 0x6b, 0xf0, 0x26, 0x5b, 0x98, 0xb5, 0xd4, 0x8a,
            0xb9, 0x19,
        ];
        assert_eq!(decrypt(&[0; 32], &[0; 12], &tag, &ct).unwrap(), [0; 16]);
        tag[0] ^= 1;
        assert!(decrypt(&[0; 32], &[0; 12], &tag, &ct).is_err());
    }
}
