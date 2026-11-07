//! Account secrets (passwords, OAuth refresh tokens).
//!
//! macOS and Windows: the OS keychain / Credential Manager. Linux (and anywhere the keychain
//! fails): `secrets.json` in the data dir, created with 0600 permissions, so a headless server
//! without a Secret Service still works.

use std::collections::BTreeMap;

use crate::paths;

const SERVICE: &str = "dev.envoy.Envoy";

/// `ENVOY_SECRETS=file` skips the OS keychain (tests, portable installs).
fn use_keychain() -> bool {
    std::env::var("ENVOY_SECRETS").map_or(true, |v| v != "file")
}

pub fn set(account_id: &str, secret: &str) -> anyhow::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if use_keychain() {
        match keyring::Entry::new(SERVICE, account_id).and_then(|e| e.set_password(secret)) {
            Ok(()) => {
                // Remove any older file copy so there is one source of truth.
                let _ = file_remove(account_id);
                return Ok(());
            }
            Err(e) => log::warn!("keychain unavailable ({e}); storing secret in file"),
        }
    }
    file_set(account_id, secret)
}

pub fn get(account_id: &str) -> Option<String> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if use_keychain() {
        if let Ok(s) = keyring::Entry::new(SERVICE, account_id).and_then(|e| e.get_password()) {
            return Some(s);
        }
    }
    file_read().remove(account_id)
}

pub fn remove(account_id: &str) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if use_keychain() {
        if let Ok(e) = keyring::Entry::new(SERVICE, account_id) {
            let _ = e.delete_credential();
        }
    }
    let _ = file_remove(account_id);
}

fn file_read() -> BTreeMap<String, String> {
    std::fs::read_to_string(paths::secrets_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn file_write(map: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let path = paths::secrets_file();
    let text = serde_json::to_string_pretty(map)?;
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
            f.write_all(text.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, text)?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn file_set(account_id: &str, secret: &str) -> anyhow::Result<()> {
    let mut map = file_read();
    map.insert(account_id.to_owned(), secret.to_owned());
    file_write(&map)
}

fn file_remove(account_id: &str) -> anyhow::Result<()> {
    let mut map = file_read();
    if map.remove(account_id).is_some() {
        file_write(&map)?;
    }
    Ok(())
}
