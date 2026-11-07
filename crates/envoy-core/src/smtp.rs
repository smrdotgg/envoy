//! Building and sending messages.

use anyhow::{Context, Result, anyhow, bail};
use lettre::message::{Mailbox as LMailbox, Mailboxes, header};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::accounts::Account;
use crate::imap::Credential;
use crate::model::Draft;

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

pub fn build(account: &Account, draft: &Draft) -> Result<Built> {
    let from: LMailbox = LMailbox::new(
        if account.display_name.is_empty() { None } else { Some(account.display_name.clone()) },
        account.email.parse().map_err(|_| anyhow!("account address {} is invalid", account.email))?,
    );
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
    let msg = b.header(header::ContentType::TEXT_PLAIN).body(draft.body.replace("\r\n", "\n").replace('\n', "\r\n")).context("couldn't build the message")?;
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

    #[test]
    fn recipients_split_outside_quotes() {
        let r = parse_recipients("\"Lima, Ana\" <ana@x.com>; bo@y.com,").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].email.to_string(), "ana@x.com");
        assert!(parse_recipients("not an address").is_err());
    }
}
