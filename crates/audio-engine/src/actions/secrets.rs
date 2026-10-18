use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;

use super::model::SecretId;

#[derive(Clone, Default)]
pub struct SecretStore {
    values: Arc<RwLock<HashMap<SecretId, String>>>,
}

impl SecretStore {
    pub fn set(&self, id: SecretId, value: String) {
        self.values.write().insert(id, value);
    }

    pub fn get(&self, id: SecretId) -> Option<String> {
        self.values
            .read()
            .get(&id)
            .cloned()
            .or_else(|| self.get_persistent(id).ok().flatten())
    }

    pub fn remove(&self, id: SecretId) {
        self.values.write().remove(&id);
    }

    pub fn set_persistent(&self, id: SecretId, value: String) -> Result<(), String> {
        save_credential(id, &value)?;
        self.set(id, value);
        Ok(())
    }

    pub fn get_persistent(&self, id: SecretId) -> Result<Option<String>, String> {
        if let Some(value) = self.values.read().get(&id).cloned() {
            return Ok(Some(value));
        }
        load_credential(id)
    }

    pub fn remove_persistent(&self, id: SecretId) -> Result<(), String> {
        delete_credential(id)?;
        self.values.write().remove(&id);
        Ok(())
    }
}

#[cfg(windows)]
fn target(id: SecretId) -> String {
    format!("VoiceMixer/Secret/{}", id.0)
}
#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
#[cfg(windows)]
fn save_credential(id: SecretId, value: &str) -> Result<(), String> {
    use windows::{
        core::PWSTR,
        Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        },
    };
    let mut name = wide(&target(id));
    let mut blob = value.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(name.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&credential, 0).map_err(|e| e.to_string()) }
}
#[cfg(windows)]
fn load_credential(id: SecretId) -> Result<Option<String>, String> {
    use std::slice;
    use windows::{
        core::PCWSTR,
        Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC},
    };
    let name = wide(&target(id));
    let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        match CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) {
            Ok(()) => {
                let credential = &*raw;
                let bytes = if credential.CredentialBlobSize == 0 {
                    Vec::new()
                } else if credential.CredentialBlob.is_null() {
                    CredFree(raw.cast());
                    return Err("CredentialBlob is null".into());
                } else {
                    slice::from_raw_parts(
                        credential.CredentialBlob,
                        credential.CredentialBlobSize as usize,
                    )
                    .to_vec()
                };
                CredFree(raw.cast());
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|e| e.to_string())
            }
            Err(error) if error.code().0 as u32 == 0x8007_0490 => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}
#[cfg(windows)]
fn delete_credential(id: SecretId) -> Result<(), String> {
    use windows::{
        core::PCWSTR,
        Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC},
    };
    let name = wide(&target(id));
    unsafe {
        match CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) {
            Ok(()) => Ok(()),
            Err(error) if error.code().0 as u32 == 0x8007_0490 => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}
#[cfg(not(windows))]
fn save_credential(_: SecretId, _: &str) -> Result<(), String> {
    Err("Credential Manager is only available on Windows".into())
}
#[cfg(not(windows))]
fn load_credential(_: SecretId) -> Result<Option<String>, String> {
    Ok(None)
}
#[cfg(not(windows))]
fn delete_credential(_: SecretId) -> Result<(), String> {
    Err("Credential Manager is only available on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(windows)]
    fn persistent_secret_survives_new_store_then_deletes() {
        let id = SecretId::new();
        let a = SecretStore::default();
        a.set_persistent(id, "credential-manager-test".into())
            .unwrap();
        let b = SecretStore::default();
        assert_eq!(b.get(id).as_deref(), Some("credential-manager-test"));
        b.remove_persistent(id).unwrap();
        let c = SecretStore::default();
        assert_eq!(c.get(id), None);
    }
}
