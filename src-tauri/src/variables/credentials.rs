pub(crate) trait CredentialStore: Send + Sync {
    fn set(&self, reference: &str, value: &str) -> Result<(), String>;
    fn get(&self, reference: &str) -> Result<String, String>;
    fn remove(&self, reference: &str) -> Result<(), String>;
}

pub(super) struct NativeCredentials;

fn entry(reference: &str) -> Result<keyring::Entry, String> {
    if !cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )) {
        return Err("Native protected storage is unavailable on this platform".into());
    }
    keyring::Entry::new("run.clai.variables", reference)
        .map_err(|_| "Native protected storage is unavailable".into())
}

impl CredentialStore for NativeCredentials {
    fn set(&self, reference: &str, value: &str) -> Result<(), String> {
        crate::assistant::auth::secrets::set_password_creating_default_collection(
            &entry(reference)?,
            value,
        )
        .map_err(|_| "Protected value could not be saved in native storage".into())
    }

    fn get(&self, reference: &str) -> Result<String, String> {
        entry(reference)?.get_password()
            .map_err(|_| "Protected value is missing or native storage is unavailable; replace it in Settings".into())
    }

    fn remove(&self, reference: &str) -> Result<(), String> {
        match entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err("Unused protected credential could not be removed".into()),
        }
    }
}
