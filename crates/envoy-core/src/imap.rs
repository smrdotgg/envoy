//! IMAP: connecting, listing mailboxes, syncing headers, flags and bodies, and replaying
//! queued changes.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::accounts::Account;
use crate::mime;
use crate::model::{Role, flags};
use crate::store::Store;

pub type Stream = TlsStream<TcpStream>;
pub type Session = async_imap::Session<Stream>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Short commands: a dead connection (laptop slept, Wi-Fi changed) is noticed quickly.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Bulk downloads, which can legitimately take a while on a slow link.
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);

pub enum Credential {
    Password(String),
    OAuth(String),
}

/// `ENVOY_INSECURE_TLS=1` accepts any server certificate: for local test servers and
/// self-signed bridges (Proton Mail Bridge). Never on by default.
pub fn insecure_tls() -> bool {
    std::env::var("ENVOY_INSECURE_TLS").is_ok_and(|v| v == "1" || v == "true")
}

pub fn tls_config() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions");
    let config = if insecure_tls() {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth()
    } else {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Arc::new(config)
}

#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn timed<T>(what: &str, f: impl std::future::Future<Output = T>) -> Result<T> {
    let limit = if what.starts_with("FETCH") { FETCH_TIMEOUT } else { COMMAND_TIMEOUT };
    tokio::time::timeout(limit, f).await.map_err(|_| anyhow!("{what} timed out (connection lost?)"))
}

struct XOAuth2(String);

impl async_imap::Authenticator for &XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        self.0.clone()
    }
}

pub async fn connect(account: &Account, cred: &Credential) -> Result<Session> {
    let host = account.imap_host.as_str();
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, account.imap_port)))
        .await
        .map_err(|_| anyhow!("Connecting to {host} timed out"))?
        .with_context(|| format!("Couldn't reach {host}:{}", account.imap_port))?;
    let _ = tcp.set_nodelay(true);
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| anyhow!("bad host name {host}"))?;
    // A bare IP still needs a name for SNI; rustls accepts IP addresses as ServerName too.
    let connector = TlsConnector::from(tls_config());
    let tls = if account.imap_port == 143 {
        // STARTTLS: greet in the clear, upgrade, then talk IMAP over TLS.
        let mut plain = async_imap::Client::new(tcp);
        timed("greeting", plain.read_response()).await?.ok_or_else(|| anyhow!("server closed the connection"))??;
        timed("STARTTLS", plain.run_command_and_check_ok("STARTTLS", None)).await??;
        let tcp = plain.into_inner();
        tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp)).await.map_err(|_| anyhow!("TLS timed out"))??
    } else {
        tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp)).await.map_err(|_| anyhow!("TLS handshake timed out"))??
    };
    let mut client = async_imap::Client::new(tls);
    if account.imap_port != 143 {
        timed("greeting", client.read_response()).await?.ok_or_else(|| anyhow!("server closed the connection"))??;
    }
    let session = match cred {
        Credential::Password(p) => timed("login", client.login(account.login(), p)).await?.map_err(|(e, _)| auth_error(e))?,
        Credential::OAuth(token) => {
            let auth = XOAuth2(crate::oauth::xoauth2(account.login(), token));
            timed("login", client.authenticate("XOAUTH2", &auth)).await?.map_err(|(e, _)| auth_error(e))?
        }
    };
    Ok(session)
}

#[derive(Debug)]
pub struct AuthError(pub String);
impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AuthError {}

fn auth_error(e: async_imap::error::Error) -> anyhow::Error {
    let raw = e.to_string();
    // async-imap renders `NO` replies as `no response: code: .., info: Some("…")`; keep the text.
    let detail = raw.split("info: Some(\"").nth(1).and_then(|r| r.rsplit_once("\")")).map(|(t, _)| t.to_owned()).unwrap_or_else(|| raw.clone());
    let detail = detail.trim_start_matches("[AUTHENTICATIONFAILED]").trim().to_owned();
    let msg = if raw.contains("Application-specific password") || raw.contains("185833") {
        "Gmail needs an app password here, not your normal password. Create one at myaccount.google.com/apppasswords (it needs 2-Step Verification), or use Google sign-in.".to_owned()
    } else if raw.contains("AUTHENTICATIONFAILED") || raw.contains("Invalid credentials") {
        format!("The server rejected the email or password ({detail}). For Gmail, use an app password rather than your normal password.")
    } else {
        format!("Sign-in failed: {detail}")
    };
    anyhow::Error::new(AuthError(msg))
}

pub fn is_auth_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AuthError>().is_some()
}

/// Decodes IMAP's modified UTF-7 mailbox names (`&AOk-t&AOk-` → `été`).
pub fn decode_utf7(name: &str) -> String {
    use base64::Engine as _;
    let mut out = String::new();
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            out.push('&');
        } else {
            let b64 = chunk.replace(',', "/");
            match base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64.trim_end_matches('=')) {
                Ok(bytes) => {
                    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
                    out.push_str(&String::from_utf16_lossy(&units));
                }
                Err(_) => out.push_str(&rest[start..start + end + 2]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

pub struct ListedMailbox {
    pub name: String,
    pub display: String,
    pub role: Role,
    pub delimiter: String,
    pub selectable: bool,
}

fn role_from_name(name: &str) -> Role {
    let lower = name.to_lowercase();
    let leaf = lower.rsplit(['/', '.']).next().unwrap_or(&lower);
    match leaf {
        "inbox" if lower == "inbox" => Role::Inbox,
        "sent" | "sent items" | "sent messages" | "sent mail" => Role::Sent,
        "drafts" | "draft" => Role::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" => Role::Trash,
        "junk" | "spam" | "junk e-mail" | "junk email" | "bulk mail" => Role::Junk,
        "archive" | "archives" => Role::Archive,
        _ => Role::Other,
    }
}

pub async fn list_mailboxes(session: &mut Session) -> Result<Vec<ListedMailbox>> {
    let names: Vec<_> = timed("LIST", async { session.list(Some(""), Some("*")).await?.try_collect::<Vec<_>>().await }).await??;
    let mut out = Vec::new();
    for n in &names {
        let mut role = Role::Other;
        let mut selectable = true;
        for a in n.attributes() {
            match a {
                NameAttribute::NoSelect => selectable = false,
                NameAttribute::All => role = Role::All,
                NameAttribute::Archive => role = Role::Archive,
                NameAttribute::Drafts => role = Role::Drafts,
                NameAttribute::Flagged => role = Role::Flagged,
                NameAttribute::Junk => role = Role::Junk,
                NameAttribute::Sent => role = Role::Sent,
                NameAttribute::Trash => role = Role::Trash,
                NameAttribute::Extension(s) => {
                    let s = s.to_ascii_lowercase();
                    if s == "\\important" {
                        role = Role::Important;
                    } else if s == "\\noselect" || s == "\\nonexistent" {
                        selectable = false;
                    }
                }
                _ => {}
            }
        }
        let name = n.name().to_owned();
        if name.eq_ignore_ascii_case("INBOX") {
            role = Role::Inbox;
        } else if role == Role::Other {
            role = role_from_name(&name);
        }
        let delimiter = n.delimiter().unwrap_or("/").to_owned();
        let decoded = decode_utf7(&name);
        let leaf = decoded.strip_prefix("[Gmail]/").or_else(|| decoded.strip_prefix("[Google Mail]/")).unwrap_or(&decoded);
        let display = if role != Role::Other && role != Role::Important { role.label().to_owned() } else { leaf.rsplit(delimiter.as_str()).next().unwrap_or(leaf).to_owned() };
        // Gmail's "[Gmail]" parent is only a container.
        if name == "[Gmail]" || name == "[Google Mail]" {
            selectable = false;
        }
        out.push(ListedMailbox { name, display, role, delimiter, selectable });
    }
    // Only one mailbox per role (the first): a server may mark two as \Sent.
    let mut seen = HashSet::new();
    for m in out.iter_mut() {
        if m.role != Role::Other && !seen.insert(m.role) {
            m.role = Role::Other;
        }
    }
    Ok(out)
}

pub fn flag_bits<'a>(it: impl Iterator<Item = Flag<'a>>) -> u32 {
    let mut f = 0;
    for flag in it {
        f |= match flag {
            Flag::Seen => flags::SEEN,
            Flag::Flagged => flags::FLAGGED,
            Flag::Answered => flags::ANSWERED,
            Flag::Draft => flags::DRAFT,
            Flag::Deleted => flags::DELETED,
            _ => 0,
        };
    }
    f
}

pub fn flag_names(bits: u32) -> String {
    let mut v = Vec::new();
    if bits & flags::SEEN != 0 {
        v.push("\\Seen");
    }
    if bits & flags::FLAGGED != 0 {
        v.push("\\Flagged");
    }
    if bits & flags::ANSWERED != 0 {
        v.push("\\Answered");
    }
    if bits & flags::DRAFT != 0 {
        v.push("\\Draft");
    }
    if bits & flags::DELETED != 0 {
        v.push("\\Deleted");
    }
    v.join(" ")
}

const HEADER_QUERY: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)])";

fn store_header(store: &Store, account: &str, mailbox: &str, f: &Fetch) -> Result<bool> {
    let (Some(uid), Some(header)) = (f.uid, f.header()) else { return Ok(false) };
    let Some(parsed) = mime::parse(header, true) else { return Ok(false) };
    let date = f.internal_date().map(|d| d.timestamp()).unwrap_or(0);
    store.upsert_header(account, mailbox, uid, flag_bits(f.flags()), f.size.unwrap_or(0), date, &parsed)?;
    Ok(true)
}

pub struct SyncOutcome {
    pub changed: bool,
    pub new_messages: u32,
}

/// Brings one mailbox's local copy up to date: new messages' headers, flag changes and
/// deletions, keeping the newest `window` messages.
/// Headers per FETCH during a first download.
const HEADER_CHUNK: u32 = 200;

fn store_fetched(store: &Mutex<Store>, account: &str, mailbox: &str, fetched: &[Fetch], above_uid: u32) -> Result<u32> {
    if fetched.is_empty() {
        return Ok(0);
    }
    let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
    st.transaction(|st| {
        let mut n = 0;
        for f in fetched {
            if f.uid.is_some_and(|u| u > above_uid) && store_header(st, account, mailbox, f)? {
                n += 1;
            }
        }
        Ok(n)
    })
}

/// Brings one mailbox's local copy up to date (see the module docs). `progress` runs after
/// each chunk of a first download lands, so the UI can show it.
pub async fn sync_mailbox(session: &mut Session, store: &Mutex<Store>, account: &str, mailbox: &str, window: u32, progress: &(dyn Fn() + Send + Sync)) -> Result<SyncOutcome> {
    let mb = timed("SELECT", session.select(mailbox)).await??;
    let uidvalidity = mb.uid_validity.unwrap_or(0);
    let mut changed = false;
    let mut new_messages = 0;
    let (local, max_uid) = {
        let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
        if st.uidvalidity(account, mailbox)? != uidvalidity {
            st.clear_mailbox(account, mailbox)?;
            st.set_uidvalidity(account, mailbox, uidvalidity)?;
            changed = true;
        }
        (st.local_uids(account, mailbox)?, st.max_uid(account, mailbox)?)
    };

    if mb.exists == 0 {
        if !local.is_empty() {
            let uids: Vec<u32> = local.iter().map(|(u, _)| *u).collect();
            store.lock().map_err(|_| anyhow!("store poisoned"))?.delete_uids(account, mailbox, &uids)?;
            changed = true;
        }
        return Ok(SyncOutcome { changed, new_messages });
    }

    if local.is_empty() {
        // First download: newest first, a chunk at a time, so the list fills in as it goes.
        let lowest = mb.exists.saturating_sub(window.max(1)) + 1;
        let mut hi = mb.exists;
        while hi >= lowest {
            let lo = hi.saturating_sub(HEADER_CHUNK - 1).max(lowest);
            let fetched: Vec<Fetch> =
                timed("FETCH headers", async { session.fetch(format!("{lo}:{hi}"), HEADER_QUERY).await?.try_collect::<Vec<_>>().await }).await??;
            let n = store_fetched(store, account, mailbox, &fetched, 0)?;
            if n > 0 {
                new_messages += n;
                changed = true;
                progress();
            }
            if lo <= 1 {
                break;
            }
            hi = lo - 1;
        }
    } else {
        let fetched: Vec<Fetch> =
            timed("FETCH new", async { session.uid_fetch(format!("{}:*", max_uid + 1), HEADER_QUERY).await?.try_collect::<Vec<_>>().await }).await??;
        let n = store_fetched(store, account, mailbox, &fetched, max_uid)?;
        if n > 0 {
            new_messages += n;
            changed = true;
        }
    }

    // Flags and deletions for what we already have.
    if let Some(&(min_uid, _)) = local.first() {
        let flags: Vec<Fetch> =
            timed("FETCH flags", async { session.uid_fetch(format!("{min_uid}:{max_uid}"), "(UID FLAGS)").await?.try_collect::<Vec<_>>().await })
                .await??;
        let server: std::collections::HashMap<u32, u32> = flags.iter().filter_map(|f| Some((f.uid?, flag_bits(f.flags())))).collect();
        changed |= store.lock().map_err(|_| anyhow!("store poisoned"))?.reconcile(account, mailbox, &local, &server, window)?;
    }
    store.lock().map_err(|_| anyhow!("store poisoned"))?.mark_synced(account, mailbox)?;
    Ok(SyncOutcome { changed, new_messages })
}

/// Unread and total counts for the sidebar.
pub async fn status(session: &mut Session, mailbox: &str) -> Result<(u32, u32)> {
    let mb = timed("STATUS", session.status(mailbox, "(MESSAGES UNSEEN)")).await??;
    Ok((mb.unseen.unwrap_or(0), mb.exists))
}

/// Downloads whole messages by UID into the store. The mailbox must be selected.
pub async fn fetch_bodies(session: &mut Session, store: &Mutex<Store>, account: &str, mailbox: &str, uids: &[u32]) -> Result<Vec<i64>> {
    if uids.is_empty() {
        return Ok(vec![]);
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let fetched: Vec<Fetch> =
        timed("FETCH bodies", async { session.uid_fetch(set, "(UID BODY.PEEK[])").await?.try_collect::<Vec<_>>().await }).await??;
    let mut ids = Vec::new();
    let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
    for f in &fetched {
        let (Some(uid), Some(raw)) = (f.uid, f.body()) else { continue };
        let Some(id) = st.id_for_uid(account, mailbox, uid)? else { continue };
        if let Some(parsed) = mime::parse(raw, false) {
            st.set_body(id, raw, &parsed)?;
            ids.push(id);
        }
    }
    Ok(ids)
}

pub async fn select(session: &mut Session, mailbox: &str) -> Result<()> {
    timed("SELECT", session.select(mailbox)).await??;
    Ok(())
}

pub async fn store_flags(session: &mut Session, mailbox: &str, uid: u32, add: u32, remove: u32) -> Result<()> {
    select(session, mailbox).await?;
    if add != 0 {
        timed("STORE", async { session.uid_store(uid.to_string(), format!("+FLAGS.SILENT ({})", flag_names(add))).await?.try_collect::<Vec<_>>().await })
            .await??;
    }
    if remove != 0 {
        timed("STORE", async {
            session.uid_store(uid.to_string(), format!("-FLAGS.SILENT ({})", flag_names(remove))).await?.try_collect::<Vec<_>>().await
        })
        .await??;
    }
    Ok(())
}

pub async fn move_message(session: &mut Session, caps: Caps, mailbox: &str, uid: u32, dest: &str) -> Result<()> {
    select(session, mailbox).await?;
    if caps.moves {
        timed("MOVE", session.uid_mv(uid.to_string(), dest)).await??;
    } else {
        timed("COPY", session.uid_copy(uid.to_string(), dest)).await??;
        expunge(session, caps, mailbox, uid, false).await?;
    }
    Ok(())
}

pub async fn expunge(session: &mut Session, caps: Caps, mailbox: &str, uid: u32, select_first: bool) -> Result<()> {
    if select_first {
        select(session, mailbox).await?;
    }
    delete_uids(session, caps, &[uid]).await
}

/// Deletes messages from the selected mailbox: flags them \Deleted and expunges them, by UID
/// with UIDPLUS, else with a plain EXPUNGE (which also takes anything else already flagged
/// \Deleted there, as other clients do on such servers).
async fn delete_uids(session: &mut Session, caps: Caps, uids: &[u32]) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    timed("STORE", async { session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await }).await??;
    if caps.uidplus {
        timed("EXPUNGE", async { session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await }).await??;
    } else {
        timed("EXPUNGE", async { session.expunge().await?.try_collect::<Vec<_>>().await }).await??;
    }
    Ok(())
}

pub async fn append(session: &mut Session, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
    timed("APPEND", session.append(mailbox, if seen { Some("(\\Seen)") } else { None }, None, raw)).await??;
    Ok(())
}

/// What the server supports, of what Envoy uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub moves: bool,
    pub idle: bool,
    /// UID EXPUNGE: expunge only the given messages.
    pub uidplus: bool,
}

pub async fn capabilities(session: &mut Session) -> Result<Caps> {
    let caps = timed("CAPABILITY", session.capabilities()).await??;
    Ok(Caps { moves: caps.has_str("MOVE"), idle: caps.has_str("IDLE"), uidplus: caps.has_str("UIDPLUS") })
}

/// Waits on IDLE for up to `wait`, returning early when the server reports a change or
/// `interrupt` fires. Gives the session back either way.
pub async fn idle(session: Session, wait: Duration, interrupt: Arc<tokio::sync::Notify>) -> Result<(Session, bool)> {
    let mut handle = session.idle();
    timed("IDLE", handle.init()).await??;
    let changed = {
        let (fut, stop) = handle.wait_with_timeout(wait);
        tokio::pin!(fut);
        let mut stop = Some(stop);
        tokio::select! {
            r = &mut fut => matches!(r?, async_imap::extensions::idle::IdleResponse::NewData(_)),
            _ = interrupt.notified() => {
                // Ask the wait to stop, and let it finish reading cleanly.
                stop.take();
                matches!(timed("IDLE stop", &mut fut).await??, async_imap::extensions::idle::IdleResponse::NewData(_))
            }
        }
    };
    let session = timed("DONE", handle.done()).await??;
    Ok((session, changed))
}

pub fn ensure(cond: bool, msg: &str) -> Result<()> {
    if !cond {
        bail!("{msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf7_decodes() {
        assert_eq!(decode_utf7("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_utf7("A&-B"), "A&B");
        assert_eq!(decode_utf7("INBOX"), "INBOX");
    }

    #[test]
    fn roles_from_names() {
        assert_eq!(role_from_name("INBOX.Sent Items"), Role::Sent);
        assert_eq!(role_from_name("Spam"), Role::Junk);
        assert_eq!(role_from_name("Projects"), Role::Other);
    }
}
