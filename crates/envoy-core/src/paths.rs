//! Where Envoy keeps its files.
//!
//! - config dir: `settings.json` and `accounts.json` (small, human-editable, safe to back up
//!   and sync between machines; no secrets in either).
//! - data dir: the local mail cache (`mail.sqlite`) and, on Linux, `secrets.json` (0600).
//!
//! `ENVOY_HOME` overrides both (everything goes under that one folder), which is handy for
//! portable installs and for tests.

use std::path::PathBuf;
use std::sync::OnceLock;

fn dirs() -> &'static (PathBuf, PathBuf) {
    static DIRS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    DIRS.get_or_init(|| {
        if let Some(home) = std::env::var_os("ENVOY_HOME") {
            let home = PathBuf::from(home);
            return (home.clone(), home);
        }
        match directories::ProjectDirs::from("dev", "envoy", "Envoy") {
            Some(p) => (p.config_dir().to_path_buf(), p.data_dir().to_path_buf()),
            None => {
                let fallback = PathBuf::from(".envoy");
                (fallback.clone(), fallback)
            }
        }
    })
}

pub fn config_dir() -> PathBuf {
    let d = dirs().0.clone();
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn data_dir() -> PathBuf {
    let d = dirs().1.clone();
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn accounts_file() -> PathBuf {
    config_dir().join("accounts.json")
}

pub fn database_file() -> PathBuf {
    data_dir().join("mail.sqlite")
}

pub fn secrets_file() -> PathBuf {
    data_dir().join("secrets.json")
}

/// Writes a file atomically: a temp file beside it, then a rename, so a crash never leaves a
/// half-written config.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
