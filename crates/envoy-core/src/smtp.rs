//! Building and sending messages.

use anyhow::{Context, Result, anyhow, bail};
use lettre::message::{Mailbox as LMailbox, Mailboxes, header};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

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

/// A draft's files, with their bytes, as [`build`] and [`build_draft`] take them.
pub type Files = [(DraftAttachment, Vec<u8>)];

/// The body: the text, and the attachments after it (multipart/mixed) if there are any.
fn with_body(b: lettre::message::MessageBuilder, text: &str, files: &Files) -> Result<Message> {
    use lettre::message::{Attachment, MultiPart, SinglePart};
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    if files.is_empty() {
        return Ok(b.header(header::ContentType::TEXT_PLAIN).body(text)?);
    }
    let mut parts = MultiPart::mixed().singlepart(SinglePart::builder().header(header::ContentType::TEXT_PLAIN).body(text));
    for (a, data) in files {
        // Text files were read as UTF-8, whatever they were in.
        let mime = if a.mime.starts_with("text/") { format!("{}; charset=utf-8", a.mime) } else { a.mime.clone() };
        let kind = header::ContentType::parse(&mime).or_else(|_| header::ContentType::parse("application/octet-stream"))?;
        parts = parts.singlepart(Attachment::new(a.filename.clone()).body(data.clone(), kind));
    }
    Ok(b.multipart(parts)?)
}

/// A draft's copy for the Drafts mailbox, with the draft's own Message-ID (see
/// `store/drafts.rs`) and its `files`. Unlike a message being sent it may have no recipients,
/// or half-typed ones (kept as typed), and it keeps Bcc, so it opens again as it was written.
pub fn build_draft(account: &Account, draft: &Draft, message_id: &str, files: &Files) -> Result<Vec<u8>> {
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
pub fn build(account: &Account, draft: &Draft, files: &Files) -> Result<Built> {
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

pub async fn send(account: &Account, cred: &Credential, from: &str, to: &[String], raw: &[u8]) -> Result<()> {
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    let host = account.smtp_host.as_str();
    let insecure = crate::imap::insecure_tls();
    let params = TlsParameters::builder(host.to_owned())
        .dangerous_accept_invalid_certs(insecure)
        .dangerous_accept_invalid_hostnames(insecure)
        .build_rustls()?;
    // 465: TLS from the first byte. Anything else: plain, upgraded with STARTTLS (required).
    let tls = if account.smtp_port == 465 || account.smtp_port == 3465 { Tls::Wrapper(params) } else { Tls::Required(params) };
    let (creds, mech) = match cred {
        Credential::Password(p) => (Credentials::new(account.login().to_owned(), p.clone()), vec![Mechanism::Plain, Mechanism::Login]),
        Credential::OAuth(t) => (Credentials::new(account.login().to_owned(), t.clone()), vec![Mechanism::Xoauth2]),
    };
    let transport: AsyncSmtpTransport<Tokio1Executor> = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(account.smtp_port)
        .tls(tls)
        .credentials(creds)
        .authentication(mech)
        .timeout(Some(std::time::Duration::from_secs(60)))
        .build();
    let envelope = lettre::address::Envelope::new(
        Some(from.parse().map_err(|_| anyhow!("bad from address"))?),
        to.iter().filter_map(|a| a.parse().ok()).collect(),
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(120), transport.send_raw(&envelope, raw))
        .await
        .map_err(|_| anyhow!("SMTP send timed out"))?
        .context("SMTP send failed")?;
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
        let raw = build_draft(&account(), &draft, "<7.abc.draft@envoy>", &[]).unwrap();
        let back = crate::mime::server_draft(&raw).unwrap();
        assert_eq!(back.notice, None, "nothing lost editing it");
        assert_eq!(Draft { account: "a".into(), ..back.draft }, draft, "{}", String::from_utf8_lossy(&raw));
        let p = crate::mime::parse(&raw, true).unwrap();
        assert_eq!(p.message_id, "<7.abc.draft@envoy>");
    }

    #[test]
    fn a_draft_copy_keeps_half_typed_recipients() {
        let draft = Draft { to: "ana@x.com, bo@".into(), subject: "Hi".into(), ..Draft::default() };
        let raw = String::from_utf8(build_draft(&account(), &draft, "<1.a.draft@envoy>", &[]).unwrap()).unwrap();
        assert!(raw.contains("To: ana@x.com, bo@"), "{raw}");
        let empty = build_draft(&account(), &Draft::default(), "<2.a.draft@envoy>", &[]).unwrap();
        assert!(crate::mime::server_draft(&empty).is_some(), "no recipients, subject or text: still a draft");
    }

    #[test]
    fn attachments_go_with_the_draft_and_the_message() {
        let pdf: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        let files = vec![
            (DraftAttachment { id: 0, filename: "plan.pdf".into(), mime: "application/pdf".into(), size: pdf.len() }, pdf.clone()),
            (DraftAttachment { id: 1, filename: "notes.txt".into(), mime: "text/plain".into(), size: 6 }, "café\n".as_bytes().to_vec()),
        ];
        let draft = Draft { to: "ana@x.com".into(), subject: "Plans".into(), body: "See attached.".into(), attachments: files.iter().map(|(a, _)| a.clone()).collect(), ..Draft::default() };
        for raw in [build_draft(&account(), &draft, "<3.a.draft@envoy>", &files).unwrap(), build(&account(), &draft, &files).unwrap().raw] {
            let back = crate::mime::server_draft(&raw).unwrap();
            assert_eq!(back.draft.body.trim_end(), "See attached.");
            assert_eq!(back.draft.attachments.iter().map(|a| (a.filename.as_str(), a.mime.as_str())).collect::<Vec<_>>(), vec![("plan.pdf", "application/pdf"), ("notes.txt", "text/plain")]);
            assert_eq!(back.files, vec![pdf.clone(), "café\n".as_bytes().to_vec()], "the bytes, exactly");
        }
    }

    #[test]
    fn recipients_split_outside_quotes() {
        let r = parse_recipients("\"Lima, Ana\" <ana@x.com>; bo@y.com,").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].email.to_string(), "ana@x.com");
        assert!(parse_recipients("not an address").is_err());
    }
}
