//! Parsing raw messages (mail-parser) into what the store and UI need.

use mail_parser::{Addr, HeaderValue, MessageParser, MimeHeaders};

use crate::model::{Address, Attachment};

pub struct Parsed {
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub date: Option<i64>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
}

fn addr(a: &Addr) -> Address {
    Address { name: a.name().unwrap_or_default().trim().to_owned(), email: a.address().unwrap_or_default().trim().to_owned() }
}

fn addresses(v: Option<&mail_parser::Address>) -> Vec<Address> {
    let Some(v) = v else { return Vec::new() };
    v.iter().map(addr).filter(|a| !a.email.is_empty() || !a.name.is_empty()).collect()
}

fn ids(v: &HeaderValue) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn angle(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() || id.starts_with('<') { id.to_owned() } else { format!("<{id}>") }
}

/// Parses headers only (or a whole message; extra parts are ignored when `headers_only`).
pub fn parse(raw: &[u8], headers_only: bool) -> Option<Parsed> {
    let msg = MessageParser::default().parse(raw)?;
    let refs: Vec<String> = ids(msg.references()).into_iter().map(|s| angle(&s)).collect();
    let irt: Vec<String> = ids(msg.in_reply_to()).into_iter().map(|s| angle(&s)).collect();
    let mut p = Parsed {
        message_id: msg.message_id().map(angle).unwrap_or_default(),
        in_reply_to: irt.first().cloned().unwrap_or_default(),
        references: refs.join(" "),
        subject: msg.subject().unwrap_or_default().trim().to_owned(),
        from: msg.from().and_then(|f| f.first()).map(addr).unwrap_or(Address { name: String::new(), email: String::new() }),
        to: addresses(msg.to()),
        cc: addresses(msg.cc()),
        reply_to: addresses(msg.reply_to()),
        date: msg.date().map(|d| d.to_timestamp()),
        text: None,
        html: None,
        attachments: Vec::new(),
    };
    if headers_only {
        return Some(p);
    }
    // Bodies: mail-parser picks the best text/plain and text/html, converting between them
    // when only one exists. Keep only what the message actually has.
    let has_html = msg.html_body.iter().any(|&i| msg.parts.get(i as usize).is_some_and(|part| part.is_text_html()));
    let has_text = msg.text_body.iter().any(|&i| msg.parts.get(i as usize).is_some_and(|part| part.is_text() && !part.is_text_html()));
    if has_text {
        p.text = msg.body_text(0).map(|t| t.into_owned());
    }
    if has_html {
        p.html = msg.body_html(0).map(|t| t.into_owned());
    }
    if p.text.is_none() && p.html.is_none() {
        p.text = msg.body_text(0).map(|t| t.into_owned());
    }
    for (index, part) in msg.attachments().enumerate() {
        let filename = part.attachment_name().map(str::to_owned).unwrap_or_else(|| format!("attachment-{}", index + 1));
        let mime = part
            .content_type()
            .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")))
            .unwrap_or_else(|| "application/octet-stream".into());
        // Inline images referenced from HTML aren't listed as files.
        let inline = part.content_disposition().is_some_and(|d| d.is_inline()) && mime.starts_with("image/") && part.content_id().is_some();
        if inline {
            continue;
        }
        p.attachments.push(Attachment { index, filename, mime, size: part.contents().len() });
    }
    Some(p)
}

/// A draft from the server (one Envoy saved, or another client's), as the composer edits it.
pub struct ServerDraft {
    /// Its account is left for the caller.
    pub draft: crate::model::Draft,
    /// The attachments' bytes, in the order of `draft.attachments`.
    pub files: Vec<Vec<u8>>,
    /// What the composer should say: whether editing it in place loses anything.
    pub notice: Option<crate::model::DraftNotice>,
}

/// Reads a draft from the server for editing: recipients, subject, plain text, threading, and
/// every attachment (inline pictures and attached messages too). What Envoy can't write back is
/// flagged rather than lost: an HTML version ([`DraftNotice::PlainText`]); or a draft that
/// can't be edited in place at all ([`DraftNotice::Copy`]: signed or encrypted, only a formatted
/// version, text in several pieces), which comes back as a copy without attachments, for the
/// original to stay as it is.
///
/// [`DraftNotice::PlainText`]: crate::model::DraftNotice::PlainText
/// [`DraftNotice::Copy`]: crate::model::DraftNotice::Copy
pub fn server_draft(raw: &[u8]) -> Option<ServerDraft> {
    use crate::model::{Draft, DraftAttachment, DraftNotice};
    let msg = MessageParser::default().parse(raw)?;
    // As typed into a recipients field: names that would split it are quoted.
    let field = |v: Option<&mail_parser::Address>| {
        addresses(v)
            .iter()
            .map(|a| if a.name.contains([',', ';', '"', '<', '>', '@']) { format!("\"{}\" <{}>", a.name.replace('"', ""), a.email) } else { a.formatted() })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let part = |i: &u32| msg.parts.get(*i as usize);
    let has_html = msg.html_body.iter().any(|i| part(i).is_some_and(|p| p.is_text_html()));
    let texts = msg.text_body.iter().filter(|i| part(i).is_some_and(|p| p.is_text() && !p.is_text_html())).count();
    // Signed or encrypted anywhere in it (a signed part beside an attachment, an S/MIME
    // signature file, PGP inline in the text): rebuilding it would drop the protection.
    const PROTECTED: &[&str] = &[
        "multipart/signed",
        "multipart/encrypted",
        "application/pkcs7-mime",
        "application/x-pkcs7-mime",
        "application/pkcs7-signature",
        "application/x-pkcs7-signature",
        "application/pgp-encrypted",
        "application/pgp-signature",
    ];
    let protected = msg.parts.iter().any(|p| {
        PROTECTED.contains(&part_type(p).as_str())
            || p.text_contents().is_some_and(|t| p.is_text() && (t.contains("-----BEGIN PGP SIGNED MESSAGE-----") || t.contains("-----BEGIN PGP MESSAGE-----")))
    });
    let copy = if protected {
        Some("it's signed or encrypted")
    } else if has_html && texts == 0 {
        Some("it only has a formatted version")
    } else if texts > 1 {
        Some("its text is in several pieces")
    } else {
        None
    };
    let refs: Vec<String> = ids(msg.references()).into_iter().map(|s| angle(&s)).collect();
    let mut draft = Draft {
        account: String::new(),
        to: field(msg.to()),
        cc: field(msg.cc()),
        bcc: field(msg.bcc()),
        subject: msg.subject().unwrap_or_default().to_owned(),
        body: msg.body_text(0).map(|t| t.replace("\r\n", "\n")).unwrap_or_default(),
        in_reply_to: ids(msg.in_reply_to()).first().map(|s| angle(s)).unwrap_or_default(),
        references: refs.join(" "),
        reply_of: None,
        attachments: Vec::new(),
    };
    if let Some(why) = copy {
        return Some(ServerDraft { draft, files: Vec::new(), notice: Some(DraftNotice::Copy(why.into())) });
    }
    let mut files = Vec::new();
    for (i, p) in msg.attachments().enumerate() {
        let data = p.contents().to_vec();
        draft.attachments.push(DraftAttachment {
            id: i as u32,
            filename: p.attachment_name().map(str::to_owned).unwrap_or_else(|| format!("attachment-{}", i + 1)),
            mime: part_type(p),
            size: data.len(),
        });
        files.push(data);
    }
    Some(ServerDraft { draft, files, notice: has_html.then_some(DraftNotice::PlainText) })
}

/// A part's `type/subtype`, lowercase.
fn part_type(p: &mail_parser::MessagePart) -> String {
    p.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")).to_ascii_lowercase()).unwrap_or_else(|| "application/octet-stream".into())
}

/// The attachments of a raw message a forward takes along: `(id, index)` pairs, each the
/// attachment's id in the draft and its index among the message's attachments (as
/// [`Attachment::index`] numbers them). Each comes back with its bytes, or why it can't.
pub fn forwarded_files(raw: &[u8], files: &[(u32, usize)]) -> Vec<Result<(crate::model::DraftAttachment, Vec<u8>), String>> {
    let Some(msg) = MessageParser::default().parse(raw) else { return files.iter().map(|_| Err("The original message can't be read".to_owned())).collect() };
    files
        .iter()
        .map(|&(id, index)| {
            let p = msg.attachments().nth(index).ok_or_else(|| "The original message doesn't have this file any more".to_owned())?;
            let data = part_bytes(&msg, p);
            let filename = p.attachment_name().map(str::to_owned).unwrap_or_else(|| format!("attachment-{}", index + 1));
            Ok((crate::model::DraftAttachment { id, filename, mime: part_type(p), size: data.len() }, data))
        })
        .collect()
}

/// A part's bytes as they were sent, transfer encoding undone. Text that doesn't name its
/// charset isn't read as UTF-8 on the way (which mangles what isn't): a file goes on as it came.
fn part_bytes(msg: &mail_parser::Message, p: &mail_parser::MessagePart) -> Vec<u8> {
    use mail_parser::{Encoding, PartType};
    let text = matches!(p.body, PartType::Text(_) | PartType::Html(_));
    if !text || p.content_type().and_then(|c| c.attribute("charset")).is_some() {
        return p.contents().to_vec();
    }
    let raw = msg.raw_message.get(p.offset_body as usize..p.offset_end as usize).unwrap_or_default();
    match p.encoding {
        Encoding::Base64 => mail_parser::decoders::base64::base64_decode(raw),
        Encoding::QuotedPrintable => mail_parser::decoders::quoted_printable::quoted_printable_decode(raw),
        Encoding::None => Some(raw.to_vec()),
    }
    .unwrap_or_else(|| p.contents().to_vec())
}

/// Writes attachment `index` of a raw message to `dir`, returning the file path.
pub fn save_attachment(raw: &[u8], index: usize, dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let msg = MessageParser::default().parse(raw).ok_or_else(|| anyhow::anyhow!("unreadable message"))?;
    let part = msg.attachments().nth(index).ok_or_else(|| anyhow::anyhow!("no such attachment"))?;
    let name = part.attachment_name().unwrap_or("attachment");
    let safe: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '_' } else { c }).collect();
    std::fs::create_dir_all(dir)?;
    let mut path = dir.join(&safe);
    let mut n = 1;
    while path.exists() {
        let (stem, ext) = match safe.rsplit_once('.') {
            Some((s, e)) => (s.to_owned(), format!(".{e}")),
            None => (safe.clone(), String::new()),
        };
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    std::fs::write(&path, part.contents())?;
    Ok(path)
}

/// The conversation a message belongs to: the first id in References (the thread's root),
/// else In-Reply-To, else its own Message-ID.
pub fn thread_id(message_id: &str, in_reply_to: &str, references: &str) -> String {
    references.split_whitespace().next().filter(|s| !s.is_empty()).unwrap_or(if in_reply_to.is_empty() { message_id } else { in_reply_to }).to_owned()
}

/// A one-line preview from a body: whitespace collapsed, quoted lines and signatures cut.
pub fn preview(text: Option<&str>, html: Option<&str>) -> String {
    let owned;
    let source = match (text, html) {
        (Some(t), _) if !t.trim().is_empty() => t,
        (_, Some(h)) => {
            owned = html_to_text(h);
            &owned
        }
        _ => return String::new(),
    };
    let mut out = String::new();
    for line in source.lines() {
        let l = line.trim();
        if l.starts_with('>') {
            continue;
        }
        if l == "--" || l == "-- " {
            break;
        }
        if l.starts_with("On ") && l.ends_with("wrote:") {
            break;
        }
        if l.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&l.split_whitespace().collect::<Vec<_>>().join(" "));
        if out.len() > 300 {
            break;
        }
    }
    truncate_chars(&out, 240)
}

pub fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

fn html_to_text(html: &str) -> String {
    // Cheap tag strip for previews only; the reader uses `html_to_markdown`.
    let mut out = String::with_capacity(html.len() / 3);
    let mut in_tag = false;
    let lower = html.to_ascii_lowercase();
    let mut skip_until: Option<&str> = None;
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if let Some(end) = skip_until {
            if lower[i..].starts_with(end) {
                i += end.len();
                skip_until = None;
            } else {
                i += 1;
            }
            continue;
        }
        let c = bytes[i];
        if c == b'<' {
            if lower[i..].starts_with("<style") {
                skip_until = Some("</style>");
                continue;
            }
            if lower[i..].starts_with("<script") {
                skip_until = Some("</script>");
                continue;
            }
            if lower[i..].starts_with("<head") {
                skip_until = Some("</head>");
                continue;
            }
            if lower[i..].starts_with("<br") || lower[i..].starts_with("<p") || lower[i..].starts_with("<div") || lower[i..].starts_with("<tr") {
                out.push('\n');
            }
            in_tag = true;
        } else if c == b'>' && in_tag {
            in_tag = false;
        } else if !in_tag {
            // Copy the whole UTF-8 character.
            let ch_len = utf8_len(c);
            if let Some(s) = html.get(i..i + ch_len) {
                out.push_str(s);
            }
            i += ch_len;
            continue;
        }
        i += 1;
    }
    decode_entities(&out)
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&zwnj;", "")
        .replace("&#8204;", "")
        .replace("&#847;", "")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&rsquo;", "\u{2019}")
        .replace("&lsquo;", "\u{2018}")
        .replace("&ldquo;", "\u{201c}")
        .replace("&rdquo;", "\u{201d}")
        .replace("&mdash;", "\u{2014}")
        .replace("&ndash;", "\u{2013}")
        .replace("&hellip;", "\u{2026}")
}

/// HTML mail to Markdown for the native reader. Layout tables become plain blocks, images
/// become their alt text (no remote loads: no tracking pixels), styles and scripts go.
pub fn html_to_markdown(html: &str) -> String {
    use htmd::HtmlToMarkdown;
    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head", "title", "meta", "link", "noscript", "svg"])
        .add_handler(vec!["img"], |el: htmd::Element| {
            let alt = el.attrs.iter().find(|a| &*a.name.local == "alt").map(|a| a.value.to_string()).unwrap_or_default();
            let alt = alt.trim();
            if alt.is_empty() || alt.len() > 80 { Some(String::new()) } else { Some(format!("[{alt}]")) }
        })
        .add_handler(vec!["table", "tbody", "thead", "tfoot", "center", "tr", "td", "th"], |el: htmd::Element| {
            let t = el.content.trim();
            if t.is_empty() { Some(String::new()) } else { Some(format!("\n\n{t}\n\n")) }
        })
        .build();
    let md = converter.convert(html).unwrap_or_else(|_| html_to_text(html));
    tidy_markdown(&md)
}

/// Collapses the blank-line runs and invisible spacer characters marketing mail is full of.
fn tidy_markdown(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    for line in md.lines() {
        let cleaned: String = line.chars().filter(|c| !matches!(c, '\u{200c}' | '\u{200b}' | '\u{034f}' | '\u{feff}' | '\u{00ad}')).collect();
        // A Markdown hard break (`<br>`) is two trailing spaces; keep it as a backslash break.
        let hard_break = cleaned.ends_with("  ") && !cleaned.trim().is_empty();
        let t = cleaned.trim_end();
        if t.trim().is_empty() || t.trim() == "\u{a0}" {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        out.push_str(t.trim_start_matches('\u{a0}'));
        if hard_break {
            out.push('\\');
        }
        out.push('\n');
    }
    // A hard break before a blank line (or the end) would draw as a literal backslash.
    let lines: Vec<&str> = out.lines().collect();
    let mut fixed = String::with_capacity(out.len());
    for (i, l) in lines.iter().enumerate() {
        let next_blank = lines.get(i + 1).is_none_or(|n| n.trim().is_empty());
        fixed.push_str(if next_blank { l.trim_end_matches('\\') } else { l });
        fixed.push('\n');
    }
    fixed.trim().to_owned()
}

/// A message's HTML made safe to open in a browser ([`crate::html::export`]): rewritten from an
/// allowlist of elements and attributes and checked by parsing it back, under a strict
/// Content-Security-Policy, with remote pictures only with `remote_images` (and only those
/// Envoy would fetch: no tracking pixels), and the message's own (`cid:`) pictures embedded as
/// `data:` URIs read from `raw`.
pub fn browser_html(html: &str, raw: Option<&[u8]>, remote_images: bool) -> String {
    use base64::Engine as _;
    let msg = raw.and_then(|r| MessageParser::default().parse(r));
    let mut budget: usize = 32 << 20;
    crate::html::export(html, remote_images, &mut |cid| {
        let want = cid.get(4..)?.trim().trim_matches(|c| c == '<' || c == '>');
        let part = msg.as_ref()?.parts.iter().find(|p| p.content_id().is_some_and(|id| id.trim_matches(|c| c == '<' || c == '>') == want))?;
        let bytes = part.contents();
        // Pictures browsers draw only (the format read from the bytes, not the part's claimed
        // type; the export keeps no other `data:` type), and not without end.
        let mime = image::guess_format(bytes).ok()?.to_mime_type();
        if !matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp") || bytes.len() > budget {
            return None;
        }
        budget -= bytes.len();
        Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
    })
}

/// The bytes of the inline part with this Content-ID (`cid:` images in HTML mail).
pub fn inline_part(raw: &[u8], cid: &str) -> Option<Vec<u8>> {
    let msg = MessageParser::default().parse(raw)?;
    let want = cid.trim_start_matches("cid:").trim_matches(|c| c == '<' || c == '>');
    msg.parts.iter().find(|p| p.content_id().is_some_and(|id| id.trim_matches(|c| c == '<' || c == '>') == want)).map(|p| p.contents().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] = b"From: Ana Lima <ana@example.com>\r\nTo: me@example.com\r\nSubject: Lunch?\r\nMessage-ID: <b@x>\r\nIn-Reply-To: <a@x>\r\nReferences: <root@x> <a@x>\r\nDate: Tue, 3 Mar 2026 10:00:00 +0000\r\nContent-Type: text/plain\r\n\r\nAre you free at noon?\r\n\r\nOn Mon, Bo wrote:\r\n> hi\r\n";

    #[test]
    fn browser_html_is_sanitized_and_carries_its_own_pictures() {
        let raw = b"From: Shop <news@shop.example>\r\nTo: me@example.com\r\nSubject: Sale\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=b\r\n\r\n--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Sale</p>\r\n--b\r\nContent-Type: image/png\r\nContent-ID: <logo@shop>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==\r\n--b\r\nContent-Type: text/plain\r\nContent-ID: <note@shop>\r\n\r\n<script>alert(1)</script>\r\n--b--\r\n";
        let html = r#"<meta http-equiv="refresh" content="0;url=https://t.example"><script>alert(1)</script><p onclick="x()">Sale <img src="cid:logo@shop"> <img src="CID:note@shop"> <img src="https://t.example/open.gif"></p>"#;
        let out = browser_html(html, Some(raw), false);
        assert!(out.contains(r#"src="data:image/png;base64,iVBORw0KGgo"#), "{out}");
        for bad in ["<script", "onclick", "refresh", "t.example", "cid:", "CID:", "text/plain"] {
            assert!(!out.contains(bad), "{bad} survived:\n{out}");
        }
        assert!(out.contains("Content-Security-Policy") && out.contains("img-src data:;"));
        let allowed = browser_html(html, Some(raw), true);
        assert!(allowed.contains("https://t.example/open.gif") && allowed.contains("img-src data: https: http:;"));
    }

    #[test]
    fn parses_headers_and_threads() {
        let p = parse(RAW, false).unwrap();
        assert_eq!(p.from.name, "Ana Lima");
        assert_eq!(p.subject, "Lunch?");
        assert_eq!(thread_id(&p.message_id, &p.in_reply_to, &p.references), "<root@x>");
        assert_eq!(preview(p.text.as_deref(), None), "Are you free at noon?");
    }

    #[test]
    fn server_drafts_say_what_editing_them_in_place_would_lose() {
        use crate::model::DraftNotice;
        let plain = b"From: a@x.com\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\nHello\r\n";
        assert_eq!(server_draft(plain).unwrap().notice, None);
        let html_only = b"From: a@x.com\r\nSubject: Hi\r\nContent-Type: text/html\r\n\r\n<p>Hello <b>there</b></p>\r\n";
        let d = server_draft(html_only).unwrap();
        assert!(matches!(d.notice, Some(DraftNotice::Copy(ref why)) if why.contains("formatted")), "{:?}", d.notice);
        assert!(d.draft.body.contains("Hello"), "the copy starts from its text");
        // Text, a picture, more text (as Apple Mail writes): one text field can't hold it.
        let pieces = b"From: a@x.com\r\nSubject: Hi\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nBefore\r\n--m\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n\r\nPNG\r\n--m\r\nContent-Type: text/plain\r\n\r\nAfter\r\n--m--\r\n";
        assert!(matches!(server_draft(pieces).unwrap().notice, Some(DraftNotice::Copy(_))));
    }

    #[test]
    fn a_draft_signed_or_encrypted_anywhere_inside_is_left_as_it_is() {
        use crate::model::DraftNotice;
        let copy = |raw: &[u8]| matches!(server_draft(raw).unwrap().notice, Some(DraftNotice::Copy(ref why)) if why.contains("signed or encrypted"));
        // A signed message with a file beside it: the signature is a level down.
        let nested = b"From: a@x.com\r\nSubject: Hi\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n--m\r\nContent-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"s\"\r\n\r\n--s\r\nContent-Type: text/plain\r\n\r\nSigned words.\r\n--s\r\nContent-Type: application/pgp-signature\r\n\r\nSIG\r\n--s--\r\n--m\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\n\r\nPDF\r\n--m--\r\n";
        assert!(copy(nested), "{:?}", server_draft(nested).unwrap().notice);
        let smime = b"From: a@x.com\r\nSubject: Hi\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nHello\r\n--m\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\n\r\nSIG\r\n--m--\r\n";
        assert!(copy(smime));
        // Signed inline (PGP in the text itself): editing would break it too.
        let inline = b"From: a@x.com\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\n-----BEGIN PGP SIGNED MESSAGE-----\r\nHash: SHA256\r\n\r\nHello\r\n-----BEGIN PGP SIGNATURE-----\r\nxyz\r\n-----END PGP SIGNATURE-----\r\n";
        assert!(copy(inline));
    }

    #[test]
    fn html_becomes_markdown() {
        let md = html_to_markdown("<html><head><style>p{}</style></head><body><table><tr><td><p>Hello <b>there</b></p><img src=x alt=\"\"><a href=\"https://x.com\">link</a></td></tr></table></body></html>");
        assert!(md.contains("Hello **there**"), "{md}");
        assert!(md.contains("[link](https://x.com)"), "{md}");
        assert!(!md.contains("p{}"));
        let md = html_to_markdown("<p>Thanks,<br>Maya</p>");
        assert!(md.contains("Thanks,\\\nMaya"), "{md:?}");
    }
}
