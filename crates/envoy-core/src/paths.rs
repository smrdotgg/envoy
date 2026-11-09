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

/// Where "Open original" and "Open in browser" put a file of type `ext` (`html`, `eml`) for
/// the app that opens it: a folder only this user can open, and whether it's the one for
/// sandboxed apps. On Linux and other Unixes it's Envoy's cache folder
/// (`~/.cache/envoy/exports`), not the shared `/tmp`; but when the default app for the type is
/// a Snap, which can't read hidden folders in home (nor the real `/tmp`), it's
/// `$XDG_DOWNLOAD_DIR/Envoy`. On macOS and Windows, a folder in the per-user temporary folder.
/// `ENVOY_HOME` keeps it under that folder. Asking for the default app runs `xdg-mime`, so call
/// this off the UI thread.
pub fn export_dir(ext: &str) -> (PathBuf, bool) {
    if let Some(home) = std::env::var_os("ENVOY_HOME") {
        return (PathBuf::from(home).join("exports"), false);
    }
    if cfg!(any(target_os = "macos", windows)) {
        return (std::env::temp_dir().join("envoy-exports"), false);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(mime) = match ext {
        "html" => Some("text/html"),
        "eml" => Some("message/rfc822"),
        _ => None,
    } && default_app(mime).is_some_and(|id| is_snap(&id, std::path::Path::new(SNAP_APPS)))
        && let Some(downloads) = directories::UserDirs::new().and_then(|u| u.download_dir().map(|d| d.join("Envoy")))
    {
        return (downloads, true);
    }
    let _ = ext;
    let dir = match directories::ProjectDirs::from("dev", "envoy", "Envoy") {
        Some(p) => p.cache_dir().join("exports"),
        None => dirs().1.join("exports"),
    };
    (dir, false)
}

/// Where snapd puts the desktop files of installed Snaps.
#[cfg(all(unix, not(target_os = "macos")))]
const SNAP_APPS: &str = "/var/lib/snapd/desktop/applications";

/// The desktop file of the default app for `mime` (`firefox_firefox.desktop`), per `xdg-mime`.
#[cfg(all(unix, not(target_os = "macos")))]
fn default_app(mime: &str) -> Option<String> {
    let out = std::process::Command::new("xdg-mime").args(["query", "default", mime]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    let id = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !id.is_empty()).then_some(id)
}

/// Whether desktop file `id` is a Snap's: snapd names them `<snap>_<app>.desktop` and lists them
/// in `snap_apps`.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn is_snap(id: &str, snap_apps: &std::path::Path) -> bool {
    let Some(stem) = id.strip_suffix(".desktop") else { return false };
    if stem.contains(['/', '\\']) || stem.starts_with('.') {
        return false;
    }
    let named = stem.split_once('_').is_some_and(|(snap, app)| !snap.is_empty() && !app.is_empty() && snap.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    named || snap_apps.join(id).is_file()
}

/// How long an export is kept: long enough for the app it was handed to.
const EXPORT_DAYS: u64 = 1;

/// Writes `bytes` to a new file in [`export_dir`], named `{stem}-{random}.{ext}`, and returns
/// its path. See [`write_private`]. In the folder for sandboxed apps (under Downloads) a
/// `.trackerignore` keeps desktop search from indexing mail. Exports older than a day are
/// removed. Runs `xdg-mime` on Linux: call it off the UI thread.
pub fn write_export(stem: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let (dir, sandboxed) = export_dir(ext);
    let path = write_private(&dir, stem, ext, bytes)?;
    if sandboxed {
        tracker_ignore(&dir)?;
    }
    prune(&dir, std::time::Duration::from_secs(EXPORT_DAYS * 86_400), &path);
    Ok(path)
}

/// An empty `.trackerignore` in `dir` (already private), created the same careful way as an
/// export, unless it's there.
fn tracker_ignore(dir: &std::path::Path) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    match opts.open(dir.join(".trackerignore")) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes `bytes` to a new file in `dir` that only this user can read. The folder is created
/// with mode 0700 and must be a real folder this user owns (not a link to one), tightened to
/// 0700 if it's looser. The file is created with mode 0600, exclusively (nothing that's already
/// there, link or file, is followed or overwritten), under a name nobody can guess (64 random
/// bits).
pub fn write_private(dir: &std::path::Path, stem: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    private_dir(dir)?;
    for _ in 0..8 {
        let path = dir.join(format!("{stem}-{:016x}.{ext}", rand::random::<u64>()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        match opts.open(&path) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "couldn't find a free file name"))
}

fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        let mut b = builder;
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b
    };
    match builder.create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::other(format!("{} isn't a folder", dir.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and can't fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other(format!("{} belongs to someone else", dir.display())));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Removes files in `dir` last changed more than `age` ago (never `keep`, never links, never
/// dot files such as `.trackerignore`).
fn prune(dir: &std::path::Path, age: std::time::Duration, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let path = e.path();
        let old = e.metadata().ok().filter(|m| m.file_type().is_file()).and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok()).is_some_and(|d| d > age);
        let dot = e.file_name().to_string_lossy().starts_with('.');
        if old && !dot && path != keep {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Writes a file atomically: a temp file beside it, then a rename, so a crash never leaves a
/// half-written config.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("envoy-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn exports_are_private_and_unguessable() {
        let base = scratch("private");
        let dir = base.join("exports");
        let a = write_private(&dir, "message-7", "eml", b"From: a@b.c\r\n\r\nhi").unwrap();
        let b = write_private(&dir, "message-7", "eml", b"again").unwrap();
        assert_ne!(a, b, "a new name each time");
        assert_eq!(std::fs::read(&a).unwrap(), b"From: a@b.c\r\n\r\nhi");
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("message-7-") && name.ends_with(".eml") && name.len() == "message-7-".len() + 16 + ".eml".len(), "{name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
            // A folder left open to others is closed again.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            write_private(&dir, "m", "html", b"x").unwrap();
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn exports_dont_follow_links() {
        let base = scratch("links");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // A link where the folder should be is refused, not followed.
        let dir = base.join("exports");
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assert!(write_private(&dir, "m", "html", b"x").is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        // A link planted at the file's name isn't written through either.
        let target = base.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let planted = base.join("planted.html");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        let mut opts = std::fs::OpenOptions::new();
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        assert!(opts.open(&planted).is_err(), "the same options as write_private");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn snap_apps_are_recognised() {
        let base = scratch("snaps");
        std::fs::write(base.join("chromium.desktop"), "").unwrap();
        for id in ["firefox_firefox.desktop", "thunderbird_thunderbird.desktop", "chromium_chromium.desktop"] {
            assert!(is_snap(id, &base), "{id}");
        }
        assert!(is_snap("chromium.desktop", &base), "listed by snapd");
        for id in ["firefox.desktop", "org.mozilla.firefox.desktop", "thunderbird.desktop", "_x.desktop", "x_.desktop", "firefox_firefox", "../etc/x_y.desktop", "Org.Foo_bar.desktop"] {
            assert!(!is_snap(id, &base), "{id}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_sandboxed_folder_is_kept_out_of_search_and_its_marker_kept() {
        let base = scratch("tracker");
        let dir = base.join("Envoy");
        let file = write_private(&dir, "message-1-safe", "html", b"x").unwrap();
        tracker_ignore(&dir).unwrap();
        tracker_ignore(&dir).unwrap();
        let marker = dir.join(".trackerignore");
        assert!(marker.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let day = std::time::Duration::from_secs(86_400);
        for p in [&marker, &file] {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(std::time::SystemTime::now() - 3 * day).unwrap();
        }
        let keep = write_private(&dir, "message-2-safe", "html", b"y").unwrap();
        prune(&dir, day, &keep);
        assert!(marker.exists() && keep.exists() && !file.exists(), "old exports go, the marker stays");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn old_exports_are_pruned() {
        let base = scratch("prune");
        let keep = write_private(&base, "new", "html", b"x").unwrap();
        let recent = write_private(&base, "recent", "html", b"x").unwrap();
        let old = write_private(&base, "old", "html", b"x").unwrap();
        let day = std::time::Duration::from_secs(86_400);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(std::time::SystemTime::now() - 3 * day).unwrap();
        prune(&base, day, &keep);
        assert!(keep.exists() && recent.exists() && !old.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
