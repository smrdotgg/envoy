//! Files attached in the composer: how big a message may be, what type a file is, and reading
//! one from disk (on the engine's threads, never the UI's). Their bytes go into the store with
//! the draft (`draft_files`, see `store/drafts.rs`), once.

use std::io::Read;
use std::path::Path;

use crate::accounts::Account;
use crate::model::DraftAttachment;

/// What a message's attachments may add up to while its server hasn't said what it takes:
/// Gmail's 25 MB (binary megabytes, as Gmail and Envoy show sizes), which is about what most
/// providers take.
pub const LIMIT: usize = 25 * 1024 * 1024;

/// How big a message may be, for an account: what its SMTP server says it takes (its EHLO
/// `SIZE`: the whole message, encoded for sending), once it has; until then, [`LIMIT`] of
/// attachments. The composer and the engine judge a message by this one rule: the composer by
/// what the message can't be less than, the engine, before it queues the message, by the
/// message itself. So the composer never refuses what the engine would send, and a message
/// over is refused before it goes, not bounced by the server after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limit {
    /// The server's `SIZE`, if it gave one (0, "no fixed limit", counts as none).
    pub server: Option<u64>,
    gmail: bool,
}

impl Limit {
    pub fn new(account: Option<&Account>, server: Option<u64>) -> Limit {
        Limit { server: server.filter(|s| *s > 0), gmail: account.is_some_and(Account::is_gmail) }
    }

    /// Who takes the message, as the composer names it.
    fn who(&self) -> &'static str {
        if self.gmail { "Gmail" } else { "your mail server" }
    }

    /// The most attachments can weigh in a message, its text taking `text` bytes: [`LIMIT`], or
    /// what fits the server's `SIZE` once encoded (as [`encoded_len`] counts).
    pub fn allowance(&self, text: usize) -> usize {
        match self.server {
            None => LIMIT,
            // 57 bytes of a file take a 78-byte line of base64.
            Some(size) => ((size as usize).saturating_sub(text) / 78) * 57,
        }
    }

    /// The biggest file that can be attached at all (to a message with no text).
    pub fn max_file(&self) -> usize {
        self.allowance(0)
    }

    /// Why a message with files of these `sizes` and text of `text` bytes can't be sent (None
    /// when it can, as far as can be told before it's built).
    pub fn over(&self, sizes: &[usize], text: usize) -> Option<String> {
        let total: usize = sizes.iter().sum();
        match self.server {
            None if total > LIMIT => {
                Some(format!("Attachments total {}, over {} {} limit. Remove some to send.", size_label(total), if self.gmail { "Gmail's" } else { "the" }, limit_label(LIMIT)))
            }
            Some(size) if sizes.iter().map(|s| encoded_len(*s)).sum::<usize>() + text > size as usize => Some(format!(
                "Attachments total {}: more than {} takes in one message (about {}). Remove some to send.",
                size_label(total),
                self.who(),
                size_label(self.allowance(text))
            )),
            _ => None,
        }
    }

    /// Why the message built for sending, `raw` bytes, can't go (None when it can): the
    /// engine's last word, before it queues it.
    pub fn refuses(&self, raw: usize) -> Option<String> {
        let size = self.server? as usize;
        (raw > size).then(|| {
            format!("This message is {} once encoded for sending; {} takes at most {}. Remove some attachments and send again.", size_label(raw), self.who(), size_label(size))
        })
    }
}

/// How big a file's bytes get once base64-encoded for sending, in lines of 76 characters and a
/// line break, as the message is written (lettre's `email-encoding`).
pub fn encoded_len(n: usize) -> usize {
    let b64 = n.div_ceil(3) * 4;
    let mut breaks = b64 / 76 * 2;
    if breaks >= 2 && b64.is_multiple_of(76) {
        breaks -= 2;
    }
    b64 + breaks
}

/// A limit as it's spoken of: `25 MB`, not `25.0 MB`.
fn limit_label(limit: usize) -> String {
    if limit >= 1024 * 1024 && limit.is_multiple_of(1024 * 1024) { format!("{} MB", limit / (1024 * 1024)) } else { size_label(limit) }
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
pub fn read_file(path: &Path, id: i64, limit: usize) -> Result<(DraftAttachment, Vec<u8>), String> {
    let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let gone = |e: std::io::Error| format!("Couldn't read {filename}: {e}");
    // Asked of the path, before opening: Windows won't open a folder as a file.
    let meta = std::fs::metadata(path).map_err(gone)?;
    if meta.is_dir() {
        return Err(format!("{filename} is a folder: attach the files in it"));
    }
    let mut file = std::fs::File::open(path).map_err(gone)?;
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
    Ok((DraftAttachment { id, size: data.len(), filename, mime, ..Default::default() }, data))
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
        assert_eq!(file, DraftAttachment { id: 3, filename: "Résumé 2026.pdf".into(), mime: "application/pdf".into(), size: 11, ..Default::default() });
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

    fn account(imap_host: &str) -> crate::accounts::Account {
        crate::accounts::Account {
            id: "a".into(),
            email: "me@x.com".into(),
            display_name: String::new(),
            auth: crate::accounts::AuthKind::Password,
            username: String::new(),
            imap_host: imap_host.into(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            hue: 0.0,
            signature: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
        }
    }

    #[test]
    fn until_the_server_says_the_limit_is_25_mb_of_attachments() {
        let gmail = account("imap.gmail.com");
        let limit = Limit::new(Some(&gmail), None);
        assert_eq!(limit.over(&[LIMIT], 10_000), None, "the limit itself is fine");
        assert_eq!(limit.over(&[20 << 20, 11 << 20], 0).as_deref(), Some("Attachments total 31.0 MB, over Gmail's 25 MB limit. Remove some to send."));
        let other = Limit::new(Some(&account("imap.fastmail.com")), Some(0));
        assert_eq!(other.over(&[26 << 20], 0).as_deref(), Some("Attachments total 26.0 MB, over the 25 MB limit. Remove some to send."), "SIZE 0 says nothing");
        assert_eq!((limit.max_file(), limit.refuses(usize::MAX)), (LIMIT, None), "and the built message isn't judged");
    }

    #[test]
    fn a_server_that_says_its_size_is_judged_by_it_encoded() {
        // Gmail's EHLO says SIZE 35882577: about 25 MB of attachments once encoded.
        let limit = Limit::new(Some(&account("imap.gmail.com")), Some(35_882_577));
        let allowance = limit.allowance(0);
        assert_eq!(size_label(allowance), "25.0 MB");
        assert!(encoded_len(allowance) <= 35_882_577 && encoded_len(allowance + 600) > 35_882_577, "{allowance} is about all that fits");
        assert_eq!(limit.over(&[allowance], 0), None);
        let over = limit.over(&[allowance / 2, allowance / 2 + 1000], 0).unwrap();
        assert!(over.starts_with("Attachments total 25.0 MB: more than Gmail takes in one message (about 25.0 MB)"), "{over}");
        assert!(limit.over(&[allowance], 5000).is_some(), "the text counts too");
        // The message built is what's judged last: its headers and parts take room too.
        assert_eq!(limit.refuses(35_882_577), None);
        let refused = limit.refuses(35_882_578).unwrap();
        assert_eq!(refused, "This message is 34.2 MB once encoded for sending; Gmail takes at most 34.2 MB. Remove some attachments and send again.");
        // A smaller server: a file it can never take is refused when it's attached.
        let small = Limit::new(Some(&account("imap.example.com")), Some(10 << 20));
        assert!(small.max_file() < 8 << 20 && small.max_file() > 7 << 20, "{}", small.max_file());
        assert!(small.over(&[9 << 20], 0).unwrap().contains("more than your mail server takes"));
    }

    #[test]
    fn encoded_sizes_are_counted_as_the_message_is_written() {
        // email-encoding's own examples.
        assert_eq!((encoded_len(0), encoded_len(16), encoded_len(300)), (0, 24, 410));
        assert_eq!(encoded_len(57), 76, "one full line, no break after it");
        assert_eq!(encoded_len(58), 76 + 2 + 4);
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(48 * 1024), "48 KB");
    }
}
