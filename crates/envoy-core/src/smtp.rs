//! Building and sending messages.

use anyhow::{Context, Result, anyhow, bail};
use lettre::message::{Mailbox as LMailbox, Mailboxes, header};
use lettre::Message;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};

use crate::accounts::Account;
use crate::imap::Credential;
use crate::model::{Draft, DraftAttachment};

/// Parses a comma/semicolon separated recipient list (`Ana <a@x.com>, b@y.com`).
pub fn parse_recipients(list: &str) -> Result<Vec<LMailbox>> {
    let mut out = Vec::new();
    for part in split_recipients(list) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mb: LMailbox = part.parse().or_else(|_| format!("<{part}>").parse()).map_err(|_| anyhow!("\"{part}\" isn't an email address"))?;
        out.push(mb);
    }
    Ok(out)
}

/// Splits on commas and semicolons outside quotes and angle brackets.
fn split_recipients(list: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut angle) = (Vec::new(), String::new(), false, false);
    for c in list.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out
}

pub struct Built {
    pub raw: Vec<u8>,
    pub from: String,
    pub to: Vec<String>,
}

fn sender(account: &Account) -> Result<LMailbox> {
    Ok(LMailbox::new(
        if account.display_name.is_empty() { None } else { Some(account.display_name.clone()) },
        account.email.parse().map_err(|_| anyhow!("account address {} is invalid", account.email))?,
    ))
}

/// A draft's files, with their bytes, as [`build`] and [`build_draft`] take them (by value:
/// each is encoded straight from its bytes, never copied first).
pub type Files = Vec<(DraftAttachment, Vec<u8>)>;

/// The body: the text, and the attachments after it (multipart/mixed) if there are any. A
/// file's name goes in its Content-Disposition, RFC 2231-encoded when it isn't plain ASCII;
/// its bytes in base64, or quoted-printable or as they are when they're text that allows it.
fn with_body(b: lettre::message::MessageBuilder, text: &str, files: Files) -> Result<Message> {
    use lettre::message::{Attachment, MultiPart, SinglePart};
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    if files.is_empty() {
        return Ok(b.header(header::ContentType::TEXT_PLAIN).body(text)?);
    }
    let mut parts = MultiPart::mixed().singlepart(SinglePart::builder().header(header::ContentType::TEXT_PLAIN).body(text));
    for (a, data) in files {
        // Text keeps the charset it came with (a forwarded file); other text that is UTF-8
        // says so; any other is left for the reader to work out, rather than mislabelled.
        let labelled = a.mime.to_ascii_lowercase().contains("charset=");
        let mime = if a.mime.starts_with("text/") && !labelled && std::str::from_utf8(&data).is_ok() { format!("{}; charset=utf-8", a.mime) } else { a.mime.clone() };
        let kind = header::ContentType::parse(&mime).or_else(|_| header::ContentType::parse("application/octet-stream"))?;
        parts = parts.singlepart(Attachment::new(a.filename).body(data, kind));
    }
    Ok(b.multipart(parts)?)
}

/// A draft's copy for the Drafts mailbox, with the draft's own Message-ID (see
/// `store/drafts.rs`) and its `files`. Unlike a message being sent it may have no recipients,
/// or half-typed ones (kept as typed), and it keeps Bcc, so it opens again as it was written.
pub fn build_draft(account: &Account, draft: &Draft, message_id: &str, files: Files) -> Result<Vec<u8>> {
    use lettre::address::Envelope;
    use lettre::message::header::{HeaderName, HeaderValue};
    let from = sender(account)?;
    let mut b = Message::builder()
        .from(from.clone())
        .subject(draft.subject.clone())
        .date_now()
        .message_id(Some(message_id.to_owned()))
        .user_agent("Envoy".into())
        .keep_bcc()
        // Never sent: the envelope only satisfies the builder.
        .envelope(Envelope::new(Some(from.email.clone()), vec![from.email.clone()])?);
    for (name, field) in [("To", &draft.to), ("Cc", &draft.cc), ("Bcc", &draft.bcc)] {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        b = match parse_recipients(field) {
            Ok(list) if !list.is_empty() => {
                let list: Mailboxes = list.into_iter().collect();
                match name {
                    "To" => b.mailbox(header::To::from(list)),
                    "Cc" => b.mailbox(header::Cc::from(list)),
                    _ => b.mailbox(header::Bcc::from(list)),
                }
            }
            _ => b.raw_header(HeaderValue::new(HeaderName::new_from_ascii_str(name), field.to_owned())),
        };
    }
    if !draft.in_reply_to.is_empty() {
        b = b.in_reply_to(draft.in_reply_to.clone());
    }
    if !draft.references.is_empty() {
        b = b.references(draft.references.clone());
    }
    let msg = with_body(b, &draft.body, files).context("couldn't build the draft")?;
    Ok(msg.formatted())
}

/// The message to send, with the draft's `files` attached.
pub fn build(account: &Account, draft: &Draft, files: Files) -> Result<Built> {
    let from = sender(account)?;
    let to = parse_recipients(&draft.to)?;
    let cc = parse_recipients(&draft.cc)?;
    let bcc = parse_recipients(&draft.bcc)?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        bail!("Add at least one recipient");
    }
    let mut b = Message::builder().from(from.clone()).subject(draft.subject.clone()).date_now();
    let host = account.email.rsplit('@').next().unwrap_or("localhost");
    b = b.message_id(Some(format!("<{}.{}@{}>", chrono::Utc::now().timestamp_millis(), rand::random::<u32>(), host)));
    if !to.is_empty() {
        b = b.mailbox(header::To::from(to.iter().cloned().collect::<Mailboxes>()));
    }
    if !cc.is_empty() {
        b = b.mailbox(header::Cc::from(cc.iter().cloned().collect::<Mailboxes>()));
    }
    if !bcc.is_empty() {
        b = b.mailbox(header::Bcc::from(bcc.iter().cloned().collect::<Mailboxes>()));
    }
    if !draft.in_reply_to.is_empty() {
        b = b.in_reply_to(draft.in_reply_to.clone());
    }
    if !draft.references.is_empty() {
        b = b.references(draft.references.clone());
    }
    b = b.user_agent("Envoy".into());
    let msg = with_body(b, &draft.body, files).context("couldn't build the message")?;
    let envelope = msg.envelope();
    let to_addrs = envelope.to().iter().map(|a| a.to_string()).collect();
    // Bcc must not appear in the stored copy: lettre's `formatted` already omits it.
    Ok(Built { raw: msg.formatted(), from: account.email.clone(), to: to_addrs })
}

/// The largest message a server takes, from its EHLO answer's `SIZE` line (RFC 1870): None when
/// it gives none, or 0 ("no fixed limit").
pub fn ehlo_size<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<u64> {
    lines
        .into_iter()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            words.next().filter(|w| w.eq_ignore_ascii_case("SIZE"))?;
            words.next().and_then(|n| n.parse().ok())
        })
        .filter(|size| *size > 0)
}

/// A connection to the account's SMTP server, encrypted (TLS from the first byte on 465,
/// STARTTLS, required, otherwise; `insecure` takes any certificate, for tests), and the largest
/// message it says it takes.
async fn connect(account: &Account, insecure: bool) -> Result<(AsyncSmtpConnection, Option<u64>)> {
    use lettre::transport::smtp::commands::Ehlo;
    use lettre::transport::smtp::extension::ClientId;
    let host = account.smtp_host.as_str();
    let params = TlsParameters::builder(host.to_owned()).dangerous_accept_invalid_certs(insecure).dangerous_accept_invalid_hostnames(insecure).build_rustls()?;
    let wrapper = account.smtp_port == 465 || account.smtp_port == 3465;
    let hello = ClientId::default();
    let mut conn = AsyncSmtpConnection::connect_tokio1((host, account.smtp_port), Some(std::time::Duration::from_secs(60)), &hello, wrapper.then(|| params.clone()), None).await?;
    if !wrapper {
        conn.starttls(params, &hello).await?;
    }
    // lettre keeps only the parts of EHLO's answer it uses: asked again for SIZE.
    let ehlo = conn.command(Ehlo::new(hello)).await?;
    Ok((conn, ehlo_size(ehlo.message())))
}

/// QUIT, without waiting long for its answer: what matters is known by then (the message
/// went, or can't), and a server slow to say goodbye mustn't turn that into a timeout.
async fn bye(conn: &mut AsyncSmtpConnection) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.quit()).await;
}

/// What the account's SMTP server says it takes (connecting, and leaving before logging in).
pub async fn server_size(account: &Account) -> Result<Option<u64>> {
    let work = async {
        let (mut conn, size) = connect(account, crate::imap::insecure_tls()).await?;
        bye(&mut conn).await;
        Ok::<_, anyhow::Error>(size)
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), work).await.map_err(|_| anyhow!("SMTP timed out"))?
}

/// A message bigger than its server takes (as the server's EHLO said, or as it answered: 552,
/// 5.3.4): never sent, and no retry will change that.
#[derive(Debug)]
pub struct TooBig {
    /// The message, encoded for sending.
    pub size: usize,
    /// What the server said it takes, if it said.
    pub limit: Option<u64>,
}

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size = crate::attach::size_label(self.size);
        match self.limit {
            Some(limit) => write!(f, "Not sent: this message is {size} once encoded for sending, and its mail server takes at most {}. Remove some attachments and send again.", crate::attach::size_label(limit as usize)),
            None => write!(f, "Not sent: its mail server refused it as too big ({size} once encoded for sending). Remove some attachments and send again."),
        }
    }
}

impl std::error::Error for TooBig {}

/// The [`TooBig`] behind a failed send, if that's what it was.
pub fn too_big(e: &anyhow::Error) -> Option<&TooBig> {
    e.chain().find_map(|c| c.downcast_ref())
}

/// How long a send may take, up to the server taking the message (QUIT comes after, on its
/// own clock: [`bye`]).
const SEND_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// Sends `raw`. `learned` gets the largest message the server says it takes, as soon as it
/// says (kept for the next message whether this one goes or not); a message over it isn't
/// sent at all ([`TooBig`]). Ok exactly when the server took the message.
pub async fn send(account: &Account, cred: &Credential, from: &str, to: &[String], raw: &[u8], learned: &mut Option<u64>) -> Result<()> {
    send_with(account, cred, from, to, raw, learned, crate::imap::insecure_tls(), SEND_BUDGET).await
}

#[allow(clippy::too_many_arguments)]
async fn send_with(account: &Account, cred: &Credential, from: &str, to: &[String], raw: &[u8], learned: &mut Option<u64>, insecure: bool, budget: std::time::Duration) -> Result<()> {
    let (creds, mech) = match cred {
        Credential::Password(p) => (Credentials::new(account.login().to_owned(), p.clone()), vec![Mechanism::Plain, Mechanism::Login]),
        Credential::OAuth(t) => (Credentials::new(account.login().to_owned(), t.clone()), vec![Mechanism::Xoauth2]),
    };
    let envelope = lettre::address::Envelope::new(
        Some(from.parse().map_err(|_| anyhow!("bad from address"))?),
        to.iter().filter_map(|a| a.parse().ok()).collect(),
    )?;
    let work = async {
        let (mut conn, size) = connect(account, insecure).await?;
        *learned = size;
        // What the server takes now, before a byte of the message goes.
        if let Some(limit) = size.filter(|l| raw.len() as u64 > *l) {
            bye(&mut conn).await;
            return Err(TooBig { size: raw.len(), limit: Some(limit) }.into());
        }
        conn.auth(&mech, &creds).await?;
        if let Err(e) = conn.send(&envelope, raw).await {
            let refused = e.status().is_some_and(|c| c.to_string() == "552") || e.to_string().contains("5.3.4");
            return Err(if refused { anyhow::Error::new(TooBig { size: raw.len(), limit: size }) } else { e.into() });
        }
        Ok::<_, anyhow::Error>(conn)
    };
    let mut conn = tokio::time::timeout(budget, work).await.map_err(|_| anyhow!("SMTP send timed out"))?.context("SMTP send failed")?;
    // Taken: sent, whatever QUIT does, however little of the budget is left. A message the
    // server took that was reported failed would go again.
    bye(&mut conn).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: "a".into(),
            email: "me@x.com".into(),
            display_name: "Me".into(),
            auth: crate::accounts::AuthKind::Password,
            username: String::new(),
            imap_host: String::new(),
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
    fn a_draft_copy_opens_again_as_it_was_written() {
        let draft = Draft {
            account: "a".into(),
            to: "\"Lima, Ana\" <ana@x.com>, bo@y.com".into(),
            cc: String::new(),
            bcc: "secret@z.com".into(),
            subject: "Café plans — Thursday".into(),
            body: "Hi Ana,\n\nThursday works. Bring the plans for the café, the long ones with all the numbers in them, so we can go through every page.\n\n-- \nMe".into(),
            in_reply_to: "<orig@x>".into(),
            references: "<root@x> <orig@x>".into(),
            reply_of: None,
            attachments: vec![],
        };
        let raw = build_draft(&account(), &draft, "<7.abc.draft@envoy>", vec![]).unwrap();
        let back = crate::mime::server_draft(&raw).unwrap();
        assert_eq!(back.notice, None, "nothing lost editing it");
        assert_eq!(Draft { account: "a".into(), ..back.draft }, draft, "{}", String::from_utf8_lossy(&raw));
        let p = crate::mime::parse(&raw, true).unwrap();
        assert_eq!(p.message_id, "<7.abc.draft@envoy>");
    }

    #[test]
    fn a_draft_copy_keeps_half_typed_recipients() {
        let draft = Draft { to: "ana@x.com, bo@".into(), subject: "Hi".into(), ..Draft::default() };
        let raw = String::from_utf8(build_draft(&account(), &draft, "<1.a.draft@envoy>", vec![]).unwrap()).unwrap();
        assert!(raw.contains("To: ana@x.com, bo@"), "{raw}");
        let empty = build_draft(&account(), &Draft::default(), "<2.a.draft@envoy>", vec![]).unwrap();
        assert!(crate::mime::server_draft(&empty).is_some(), "no recipients, subject or text: still a draft");
    }

    #[test]
    fn attachments_go_with_the_draft_and_the_message() {
        let pdf: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        let files = vec![
            (DraftAttachment { id: 0, filename: "plan.pdf".into(), mime: "application/pdf".into(), size: pdf.len(), ..Default::default() }, pdf.clone()),
            (DraftAttachment { id: 1, filename: "notes.txt".into(), mime: "text/plain".into(), size: 6, ..Default::default() }, "café\n".as_bytes().to_vec()),
        ];
        let draft = Draft { to: "ana@x.com".into(), subject: "Plans".into(), body: "See attached.".into(), attachments: files.iter().map(|(a, _)| a.clone()).collect(), ..Draft::default() };
        for raw in [build_draft(&account(), &draft, "<3.a.draft@envoy>", files.clone()).unwrap(), build(&account(), &draft, files.clone()).unwrap().raw] {
            let back = crate::mime::server_draft(&raw).unwrap();
            assert_eq!(back.draft.body.trim_end(), "See attached.");
            assert_eq!(back.draft.attachments.iter().map(|a| (a.filename.as_str(), a.mime.as_str())).collect::<Vec<_>>(), vec![("plan.pdf", "application/pdf"), ("notes.txt", "text/plain; charset=utf-8")], "the charset said, once");
            assert_eq!(back.files, vec![pdf.clone(), "café\n".as_bytes().to_vec()], "the bytes, exactly");
        }
    }

    /// A file as the composer attaches it: its type from its name.
    fn attached(id: i64, name: &str, data: &[u8]) -> (DraftAttachment, Vec<u8>) {
        (DraftAttachment { id, filename: name.into(), mime: crate::attach::type_for(name).into(), size: data.len(), ..Default::default() }, data.to_vec())
    }

    fn message_with(files: Files) -> Vec<u8> {
        let draft = Draft { to: "ana@x.com".into(), subject: "Files".into(), body: "Attached.".into(), attachments: files.iter().map(|(a, _)| a.clone()).collect(), ..Draft::default() };
        build(&account(), &draft, files).unwrap().raw
    }

    #[test]
    fn non_ascii_file_names_are_encoded_and_come_back_whole() {
        let long = format!("Протокол встречи {} — итоги.pdf", "очень длинное название ".repeat(4));
        let names = ["Résumé 2026.pdf", "会议纪要.docx", "naïve \"quoted\"; name.txt", long.as_str()];
        let files: Files = names.iter().enumerate().map(|(i, n)| attached(i as i64, n, b"data")).collect();
        let raw = message_with(files);
        let text = String::from_utf8(raw.clone()).expect("the message is ASCII");
        assert!(text.is_ascii(), "headers are encoded, never raw UTF-8");
        assert!(text.contains("filename*0*=utf-8''") || text.contains("filename*=utf-8''"), "RFC 2231: {text}");
        let back = crate::mime::server_draft(&raw).unwrap();
        assert_eq!(back.draft.attachments.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(), names);
        let parsed = crate::mime::parse(&raw, false).unwrap();
        assert_eq!(parsed.attachments.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(), names, "and as the reader lists them");
    }

    #[test]
    fn each_file_goes_with_the_type_its_name_says() {
        let raw = message_with(vec![attached(0, "photo.JPG", &[0xff, 0xd8, 0xff]), attached(1, "deck.pptx", b"PK"), attached(2, "unknown.zzz", b"?")]);
        let back = crate::mime::server_draft(&raw).unwrap();
        let types: Vec<&str> = back.draft.attachments.iter().map(|a| a.mime.as_str()).collect();
        assert_eq!(types, ["image/jpeg", "application/vnd.openxmlformats-officedocument.presentationml.presentation", "application/octet-stream"]);
    }

    #[test]
    fn text_files_say_utf_8_only_when_they_are() {
        let latin1 = b"caf\xe9 cr\xe8me\n".to_vec();
        let raw = message_with(vec![attached(0, "menu.txt", &latin1), attached(1, "notes.txt", "café\n".as_bytes())]);
        let text = String::from_utf8_lossy(&raw);
        assert_eq!(text.matches("charset=utf-8").count(), 2, "the body and notes.txt, not menu.txt: {text}");
        // Forwarded on, each is the bytes it was.
        let back = crate::mime::forwarded_files(&raw, &[(0, 0), (1, 1)]);
        assert_eq!(back[0].as_ref().unwrap().1, latin1);
        assert_eq!(back[1].as_ref().unwrap().1, "café\n".as_bytes());
    }

    #[test]
    fn a_large_file_is_encoded_in_short_lines_and_comes_back_exact() {
        let big: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let raw = message_with(vec![attached(0, "video.mov", &big)]);
        assert!(raw.len() < big.len() * 14 / 10, "base64, not anything bigger: {} for {}", raw.len(), big.len());
        assert!(raw.split(|b| *b == b'\n').all(|l| l.len() <= 1000), "no line over SMTP's 1000");
        let back = crate::mime::forwarded_files(&raw, &[(0, 0)]);
        assert!(back[0].as_ref().unwrap().1 == big, "the bytes, exactly");
    }

    #[test]
    fn the_servers_size_is_read_from_its_ehlo() {
        let gmail = ["smtp.gmail.com at your service, [203.0.113.7]", "SIZE 35882577", "8BITMIME", "AUTH LOGIN PLAIN XOAUTH2", "ENHANCEDSTATUSCODES", "PIPELINING", "CHUNKING", "SMTPUTF8"];
        assert_eq!(ehlo_size(gmail), Some(35_882_577));
        assert_eq!(ehlo_size(["mx.example.com", "size 1000000"]), Some(1_000_000), "any case");
        assert_eq!(ehlo_size(["mx.example.com", "SIZE 0"]), None, "no fixed limit");
        assert_eq!(ehlo_size(["mx.example.com", "SIZE"]), None, "none given");
        assert_eq!(ehlo_size(["mx.example.com", "SIZEABLE 7", "8BITMIME"]), None);
    }

    /// An SMTP server on localhost for one connection, STARTTLS and all (a throwaway
    /// certificate): its EHLO says `ehlo`, it answers the message's end with `end`, and QUIT
    /// only if `bye` (otherwise it never answers it). Returns its port, and the commands it got.
    async fn fake_server(ehlo: &'static str, end: &'static str, bye: bool) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        fake_server_slow(ehlo, end, bye, std::time::Duration::ZERO).await
    }

    /// [`fake_server`], taking `end_after` to answer the message's end.
    async fn fake_server_slow(ehlo: &'static str, end: &'static str, bye: bool, end_after: std::time::Duration) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cert = rustls::pki_types::CertificateDer::from(include_bytes!("../testdata/smtp-test-cert.der").to_vec());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(include_bytes!("../testdata/smtp-test-key.der").to_vec()));
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut plain = BufReader::new(stream);
            let mut line = String::new();
            plain.get_mut().write_all(b"220 localhost\r\n").await.unwrap();
            plain.read_line(&mut line).await.unwrap();
            plain.get_mut().write_all(b"250-localhost\r\n250 STARTTLS\r\n").await.unwrap();
            plain.read_line(&mut line).await.unwrap();
            plain.get_mut().write_all(b"220 Ready\r\n").await.unwrap();
            let mut conn = BufReader::new(tokio_rustls::TlsAcceptor::from(Arc::new(tls)).accept(plain.into_inner()).await.unwrap());
            let (mut got, mut data) = (Vec::new(), false);
            loop {
                line.clear();
                if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                if data {
                    if line == ".\r\n" {
                        data = false;
                        tokio::time::sleep(end_after).await;
                        conn.get_mut().write_all(end.as_bytes()).await.unwrap();
                    }
                    continue;
                }
                got.push(line.trim_end().to_owned());
                let answer: &[u8] = match line.as_str() {
                    l if l.starts_with("EHLO ") => ehlo.as_bytes(),
                    l if l.starts_with("AUTH ") => b"235 Authenticated\r\n",
                    "DATA\r\n" => {
                        data = true;
                        b"354 Go on\r\n"
                    }
                    "QUIT\r\n" => {
                        if bye {
                            let _ = conn.get_mut().write_all(b"221 Bye\r\n").await;
                        } else {
                            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                        }
                        break;
                    }
                    _ => b"250 OK\r\n",
                };
                conn.get_mut().write_all(answer).await.unwrap();
            }
            got
        });
        (port, task)
    }

    const BIG: &[u8] = b"From: me@x.com\r\nTo: you@x.com\r\nSubject: Big\r\n\r\nMore than ten bytes.\r\n";

    async fn send_to(port: u16, learned: &mut Option<u64>) -> Result<()> {
        send_within(port, learned, SEND_BUDGET).await
    }

    async fn send_within(port: u16, learned: &mut Option<u64>, budget: std::time::Duration) -> Result<()> {
        let local = Account { smtp_host: "127.0.0.1".into(), smtp_port: port, ..account() };
        send_with(&local, &Credential::Password("pw".into()), "me@x.com", &["you@x.com".into()], BIG, learned, true, budget).await
    }

    #[tokio::test]
    async fn a_message_over_what_the_server_says_now_never_goes() {
        let (port, server) = fake_server("250-localhost\r\n250-SIZE 10\r\n250 AUTH PLAIN LOGIN\r\n", "250 OK\r\n", true).await;
        let mut learned = None;
        let err = send_to(port, &mut learned).await.unwrap_err();
        let refused = too_big(&err).expect("too big");
        assert_eq!((refused.size, refused.limit, learned), (BIG.len(), Some(10), Some(10)), "and what it said is kept");
        let got = server.await.unwrap();
        assert!(!got.iter().any(|c| c.starts_with("AUTH") || c.starts_with("MAIL")), "not a byte of it: {got:?}");
    }

    #[tokio::test]
    async fn a_server_refusing_a_message_as_too_big_is_heard_as_that() {
        let (port, server) = fake_server("250-localhost\r\n250 AUTH PLAIN LOGIN\r\n", "552 5.3.4 Message size exceeds fixed maximum message size\r\n", true).await;
        let mut learned = None;
        let err = send_to(port, &mut learned).await.unwrap_err();
        assert_eq!(too_big(&err).map(|t| (t.size, t.limit)), Some((BIG.len(), None)), "{err:#}");
        assert!(err.to_string().contains("SMTP send failed") && format!("{err:#}").contains("refused it as too big"), "{err:#}");
        server.await.unwrap();
        // Any other refusal is just that.
        let (port, server) = fake_server("250-localhost\r\n250 AUTH PLAIN LOGIN\r\n", "451 4.3.0 Try again later\r\n", true).await;
        let err = send_to(port, &mut learned).await.unwrap_err();
        assert!(too_big(&err).is_none(), "{err:#}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_server_that_never_says_goodbye_changes_nothing() {
        let quick = std::time::Duration::from_secs(10);
        // Too big: said so at once, not after QUIT's answer that never comes.
        let (port, _server) = fake_server("250-localhost\r\n250-SIZE 10\r\n250 AUTH PLAIN LOGIN\r\n", "250 OK\r\n", false).await;
        let err = tokio::time::timeout(quick, send_to(port, &mut None)).await.expect("told at once").unwrap_err();
        assert!(too_big(&err).is_some(), "{err:#}");
        // Sent: it went, whatever QUIT does (a send reported failed would go again).
        let (port, _server) = fake_server("250-localhost\r\n250 AUTH PLAIN LOGIN\r\n", "250 OK\r\n", false).await;
        tokio::time::timeout(quick, send_to(port, &mut None)).await.expect("told at once").unwrap();
    }

    #[tokio::test]
    async fn a_message_taken_as_the_budget_runs_out_is_sent_whatever_quit_does() {
        // Taken with under a second of the budget left, then QUIT never answered: QUIT has its
        // own few seconds, after the budget, so the send is reported as what it was.
        let budget = std::time::Duration::from_secs(3);
        let (port, _server) = fake_server_slow("250-localhost\r\n250 AUTH PLAIN LOGIN\r\n", "250 OK\r\n", false, std::time::Duration::from_millis(2200)).await;
        let started = std::time::Instant::now();
        send_within(port, &mut None, budget).await.expect("taken, so sent");
        assert!(started.elapsed() > budget, "QUIT ran past the budget, and that changed nothing");
    }

    #[tokio::test]
    async fn a_send_learns_what_its_server_takes() {
        let (port, server) = fake_server("250-localhost\r\n250-SIZE 35882577\r\n250 AUTH PLAIN LOGIN\r\n", "250 OK\r\n", true).await;
        let mut learned = None;
        send_to(port, &mut learned).await.unwrap();
        assert_eq!(learned, Some(35_882_577));
        assert!(server.await.unwrap().iter().any(|c| c.starts_with("MAIL FROM")));
    }

    #[test]
    fn recipients_split_outside_quotes() {
        let r = parse_recipients("\"Lima, Ana\" <ana@x.com>; bo@y.com,").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].email.to_string(), "ana@x.com");
        assert!(parse_recipients("not an address").is_err());
    }
}
