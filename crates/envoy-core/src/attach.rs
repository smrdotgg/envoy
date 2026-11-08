//! Files attached in the composer: how big a message's files may be, what type a file is, and
//! reading one from disk (on the engine's threads, never the UI's). Their bytes go into the
//! store with the draft (`draft_files`, see `store/drafts.rs`), once.

use std::io::Read;
use std::path::Path;

use crate::accounts::Account;
use crate::model::DraftAttachment;

/// The most a message's attachments may add up to: Gmail's 25 MB (in the binary megabytes
/// Gmail and Envoy both show; Gmail's server takes a message just over that once encoded). It's
/// what most providers take, so it's every account's limit: a message over it is refused before
/// it's sent, rather than bounced by the server after.
pub const LIMIT: usize = 25 * 1024 * 1024;

/// Whose limit [`LIMIT`] is, for an account, as a message names it: "Gmail's 25 MB limit".
fn whose(account: Option<&Account>) -> &'static str {
    if account.is_some_and(Account::is_gmail) { "Gmail's" } else { "the" }
}

/// What the composer says when the files add up to more than [`LIMIT`] (None when they don't).
pub fn over_limit(account: Option<&Account>, total: usize) -> Option<String> {
    (total > LIMIT).then(|| format!("Attachments total {}, over {} {} limit. Remove some to send.", size_label(total), whose(account), limit_label(LIMIT)))
}

/// A limit as it's spoken of: `25 MB`, not `25.0 MB`.
fn limit_label(limit: usize) -> String {
    if limit >= 1024 * 1024 && limit.is_multiple_of(1024 * 1024) { format!("{} MB", limit / (1024 * 1024)) } else { size_label(limit) }
}

/// The files' total size.
pub fn total(files: &[DraftAttachment]) -> usize {
    files.iter().map(|f| f.size).sum()
}

/// A size as the composer and the reader show it: `512 B`, `48 KB`, `3.2 MB`.
pub fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    }
}

/// Reads a file to attach, as attachment `id`. Refuses a folder, and a file bigger than `limit`
/// without reading it (it could never be sent). The error is the sentence to show.
pub fn read_file(path: &Path, id: u32, limit: usize) -> Result<(DraftAttachment, Vec<u8>), String> {
    let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let gone = |e: std::io::Error| format!("Couldn't read {filename}: {e}");
    let mut file = std::fs::File::open(path).map_err(gone)?;
    let meta = file.metadata().map_err(gone)?;
    if meta.is_dir() {
        return Err(format!("{filename} is a folder: attach the files in it"));
    }
    let too_big = |size: usize| too_big(&filename, size, limit);
    let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    if len > limit {
        return Err(too_big(len));
    }
    // It may have grown since: never more than the limit (and one byte to tell).
    let mut data = Vec::with_capacity(len);
    file.by_ref().take(limit as u64 + 1).read_to_end(&mut data).map_err(gone)?;
    if data.len() > limit {
        return Err(too_big(data.len()));
    }
    let mime = type_for(&filename).to_owned();
    Ok((DraftAttachment { id, size: data.len(), filename, mime }, data))
}

/// Why a file of `size` bytes can't be attached, under `limit`.
pub fn too_big(filename: &str, size: usize, limit: usize) -> String {
    format!("{filename} is {}: more than a message can carry ({})", size_label(size), limit_label(limit))
}

/// A file's MIME type from its extension; `application/octet-stream` for one not known here.
/// Mail clients go by this to show or open a file, so the common ones are covered.
pub fn type_for(filename: &str) -> &'static str {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        // Documents
        "pdf" => "application/pdf",
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "xml" => "application/xml",
        "json" => "application/json",
        "rtf" => "application/rtf",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        "eml" => "message/rfc822",
        "epub" => "application/epub+zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "pages" => "application/vnd.apple.pages",
        "numbers" => "application/vnd.apple.numbers",
        "key" => "application/vnd.apple.keynote",
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        // Audio and video
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        // Archives
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("envoy-attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn types_come_from_the_extension_whatever_its_case() {
        assert_eq!(type_for("Q3 report.PDF"), "application/pdf");
        assert_eq!(type_for("photo.jpeg"), "image/jpeg");
        assert_eq!(type_for("notes.txt"), "text/plain");
        assert_eq!(type_for("deck.pptx"), "application/vnd.openxmlformats-officedocument.presentationml.presentation");
        assert_eq!(type_for("backup.tar.gz"), "application/gzip");
        assert_eq!(type_for("Makefile"), "application/octet-stream");
        assert_eq!(type_for("archive.unknownext"), "application/octet-stream");
        assert_eq!(type_for(".bashrc"), "application/octet-stream");
    }

    #[test]
    fn a_file_is_read_with_its_name_type_and_size() {
        let dir = temp("read");
        let path = dir.join("Résumé 2026.pdf");
        std::fs::write(&path, b"%PDF-1.7 \x00\xff").unwrap();
        let (file, data) = read_file(&path, 3, LIMIT).unwrap();
        assert_eq!(file, DraftAttachment { id: 3, filename: "Résumé 2026.pdf".into(), mime: "application/pdf".into(), size: 11 });
        assert_eq!(data, b"%PDF-1.7 \x00\xff");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_missing_files_and_files_over_the_limit_are_refused() {
        let dir = temp("refuse");
        let err = read_file(&dir, 0, LIMIT).unwrap_err();
        assert!(err.contains("is a folder"), "{err}");
        let err = read_file(&dir.join("gone.txt"), 0, LIMIT).unwrap_err();
        assert!(err.starts_with("Couldn't read gone.txt"), "{err}");
        let big = dir.join("video.mov");
        std::fs::write(&big, vec![7u8; 2048]).unwrap();
        let err = read_file(&big, 0, 1024).unwrap_err();
        assert_eq!(err, "video.mov is 2 KB: more than a message can carry (1 KB)");
        assert!(read_file(&big, 0, 2048).is_ok(), "exactly the limit is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_limit_is_named_for_gmail_and_said_plainly_otherwise() {
        let mut gmail = crate::accounts::Account {
            id: "a".into(),
            email: "me@gmail.com".into(),
            display_name: String::new(),
            auth: crate::accounts::AuthKind::Password,
            username: String::new(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            hue: 0.0,
            signature: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
        };
        assert_eq!(over_limit(Some(&gmail), LIMIT), None, "the limit itself is fine");
        assert_eq!(over_limit(Some(&gmail), 31 * 1024 * 1024).as_deref(), Some("Attachments total 31.0 MB, over Gmail's 25 MB limit. Remove some to send."));
        gmail.imap_host = "imap.fastmail.com".into();
        assert_eq!(over_limit(Some(&gmail), 26 * 1024 * 1024).as_deref(), Some("Attachments total 26.0 MB, over the 25 MB limit. Remove some to send."));
        assert!(over_limit(None, LIMIT + 1).is_some(), "a byte over is over");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(48 * 1024), "48 KB");
    }
}
