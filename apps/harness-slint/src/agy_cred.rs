//! Windows Credential Manager store for Antigravity accounts (generic credentials, copied field by field).
//! Secrets are never logged; buffers Harness allocates are zeroed after use.

use harness_agent_hub::SharedAgyCredStore;

/// The OS credential store for the hub.
pub fn shared() -> SharedAgyCredStore {
    #[cfg(windows)]
    {
        SharedAgyCredStore(std::sync::Arc::new(win::WinCredStore))
    }
    #[cfg(not(windows))]
    {
        SharedAgyCredStore::default()
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use harness_agent_hub::agy_cred::zero;
    use harness_agent_hub::{AgyCredStore, CredAttribute, CredBlob};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND, FILETIME};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CREDENTIAL_ATTRIBUTEW,
        CRED_PERSIST_LOCAL_MACHINE, CRED_PERSIST_SESSION, CRED_TYPE_GENERIC,
    };

    pub struct WinCredStore;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_opt(s: Option<&String>) -> Option<Vec<u16>> {
        s.map(|s| wide(s))
    }

    fn ptr_of(buf: &mut Option<Vec<u16>>) -> *mut u16 {
        buf.as_mut().map_or(std::ptr::null_mut(), Vec::as_mut_ptr)
    }

    unsafe fn from_wide(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut len = 0;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        Some(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(p, len)
        }))
    }

    unsafe fn bytes(p: *const u8, len: u32) -> Vec<u8> {
        if p.is_null() || len == 0 {
            return Vec::new();
        }
        unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec()
    }

    fn last_error(what: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("{what} failed (Windows error {code})")
    }

    impl AgyCredStore for WinCredStore {
        fn available(&self) -> bool {
            true
        }

        fn read(&self, target: &str) -> Result<Option<CredBlob>, String> {
            let name = wide(target);
            let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
            if unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut raw) } == 0 {
                if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                    return Ok(None);
                }
                return Err(last_error("CredReadW"));
            }
            let cred = unsafe { &*raw };
            let mut blob = CredBlob::default();
            blob.persist = cred.Persist;
            blob.user_name = unsafe { from_wide(cred.UserName) };
            blob.comment = unsafe { from_wide(cred.Comment) };
            blob.target_alias = unsafe { from_wide(cred.TargetAlias) };
            blob.secret = unsafe { bytes(cred.CredentialBlob, cred.CredentialBlobSize) };
            if !cred.Attributes.is_null() {
                for i in 0..cred.AttributeCount as usize {
                    let a = unsafe { &*cred.Attributes.add(i) };
                    blob.attributes.push(CredAttribute {
                        keyword: unsafe { from_wide(a.Keyword) }.unwrap_or_default(),
                        flags: a.Flags,
                        value: unsafe { bytes(a.Value, a.ValueSize) },
                    });
                }
            }
            if !cred.CredentialBlob.is_null() {
                unsafe {
                    std::ptr::write_bytes(cred.CredentialBlob, 0, cred.CredentialBlobSize as usize);
                }
            }
            unsafe { CredFree(raw.cast()) };
            Ok(Some(blob))
        }

        fn write(&self, target: &str, blob: &CredBlob) -> Result<(), String> {
            let mut name = wide(target);
            let mut user = wide_opt(blob.user_name.as_ref());
            let mut comment = wide_opt(blob.comment.as_ref());
            let mut alias = wide_opt(blob.target_alias.as_ref());
            let mut secret = blob.secret.clone();
            let mut keywords: Vec<Vec<u16>> =
                blob.attributes.iter().map(|a| wide(&a.keyword)).collect();
            let mut values: Vec<Vec<u8>> =
                blob.attributes.iter().map(|a| a.value.clone()).collect();
            let mut attributes: Vec<CREDENTIAL_ATTRIBUTEW> = blob
                .attributes
                .iter()
                .zip(keywords.iter_mut().zip(values.iter_mut()))
                .map(|(a, (k, v))| CREDENTIAL_ATTRIBUTEW {
                    Keyword: k.as_mut_ptr(),
                    Flags: a.flags,
                    ValueSize: u32::try_from(v.len()).unwrap_or(0),
                    Value: v.as_mut_ptr(),
                })
                .collect();
            let persist = if blob.persist == CRED_PERSIST_SESSION || blob.persist == 0 {
                CRED_PERSIST_LOCAL_MACHINE
            } else {
                blob.persist
            };
            let cred = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: name.as_mut_ptr(),
                Comment: ptr_of(&mut comment),
                LastWritten: FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                },
                CredentialBlobSize: u32::try_from(secret.len()).map_err(|e| e.to_string())?,
                CredentialBlob: if secret.is_empty() {
                    std::ptr::null_mut()
                } else {
                    secret.as_mut_ptr()
                },
                Persist: persist,
                AttributeCount: u32::try_from(attributes.len()).map_err(|e| e.to_string())?,
                Attributes: if attributes.is_empty() {
                    std::ptr::null_mut()
                } else {
                    attributes.as_mut_ptr()
                },
                TargetAlias: ptr_of(&mut alias),
                UserName: ptr_of(&mut user),
            };
            let ok = unsafe { CredWriteW(&cred, 0) } != 0;
            let result = if ok {
                Ok(())
            } else {
                Err(last_error("CredWriteW"))
            };
            zero(&mut secret);
            for v in &mut values {
                zero(v);
            }
            result
        }

        fn delete(&self, target: &str) -> Result<(), String> {
            let name = wide(target);
            if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
                return Ok(());
            }
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(());
            }
            Err(last_error("CredDeleteW"))
        }
    }
}
