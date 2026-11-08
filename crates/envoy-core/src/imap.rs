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

/// A request that can't succeed as it is, whatever the server's state (say, a draft that
/// can't be built into a message): [`failure`] reads it as [`Failure::Permanent`].
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// What kind of failure a command met, to decide whether to keep quiet and retry.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// Network trouble, or a server saying "not now" (UNAVAILABLE, INUSE, SERVERBUG): it
    /// clears by itself.
    Temporary,
    /// Refused, and it would be again until someone acts: BAD, or NO with OVERQUOTA,
    /// TRYCREATE, NOPERM and the like (RFC 5530), or [`Refused`].
    Permanent,
    /// Refused without saying why.
    Unknown,
}

pub fn failure(e: &anyhow::Error) -> Failure {
    const NOW: &[&str] = &["UNAVAILABLE", "INUSE", "SERVERBUG", "EXPUNGEISSUED"];
    const FOR_GOOD: &[&str] =
        &["TRYCREATE", "OVERQUOTA", "NONEXISTENT", "NOPERM", "CANNOT", "LIMIT", "AUTHORIZATIONFAILED", "PARSE", "PRIVACYREQUIRED", "CONTACTADMIN", "ALREADYEXISTS"];
    if e.downcast_ref::<Refused>().is_some() || is_auth_error(e) {
        return Failure::Permanent;
    }
    match e.downcast_ref::<async_imap::error::Error>() {
        Some(async_imap::error::Error::Bad(_)) => Failure::Permanent,
        Some(async_imap::error::Error::No(what)) => {
            let what = what.to_ascii_uppercase();
            if NOW.iter().any(|c| what.contains(c)) {
                Failure::Temporary
            } else if FOR_GOOD.iter().any(|c| what.contains(c)) {
                Failure::Permanent
            } else {
                Failure::Unknown
            }
        }
        // Lost connections, timeouts, I/O errors, a folder list not loaded yet.
        _ => Failure::Temporary,
    }
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

/// An IMAP quoted string.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// What a session runs over: TLS to the server in the app, an in-memory pipe in tests.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send> Io for T {}

pub async fn select(session: &mut async_imap::Session<impl Io>, mailbox: &str) -> Result<()> {
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

pub async fn move_message(session: &mut async_imap::Session<impl Io>, caps: Caps, mailbox: &str, uid: u32, dest: &str) -> Result<()> {
    select(session, mailbox).await?;
    if caps.moves {
        timed("MOVE", session.uid_mv(uid.to_string(), dest)).await??;
    } else {
        // async-imap sends COPY's mailbox as given: quoted here, for names with spaces.
        timed("COPY", session.uid_copy(uid.to_string(), quoted(dest))).await??;
        delete_uids(session, caps, &[uid]).await?;
    }
    Ok(())
}

pub async fn expunge(session: &mut async_imap::Session<impl Io>, caps: Caps, mailbox: &str, uid: u32, select_first: bool) -> Result<()> {
    if select_first {
        select(session, mailbox).await?;
    }
    delete_uids(session, caps, &[uid]).await
}

/// Deletes these messages from the selected mailbox: flags them \Deleted and, with UIDPLUS,
/// expunges exactly them (UID EXPUNGE). Without UIDPLUS they stay flagged, for the server or
/// the user to remove: a plain EXPUNGE would also destroy anything else flagged \Deleted there,
/// by the user or another client, so Envoy never sends one. Envoy's lists hide \Deleted mail.
async fn delete_uids(session: &mut async_imap::Session<impl Io>, caps: Caps, uids: &[u32]) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    timed("STORE", async { session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await }).await??;
    if caps.uidplus {
        timed("EXPUNGE", async { session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await }).await??;
    }
    Ok(())
}

pub async fn append(session: &mut Session, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
    timed("APPEND", session.append(mailbox, if seen { Some("(\\Seen)") } else { None }, None, raw)).await??;
    Ok(())
}

/// What saving a draft did on the server ([`replace_draft`]).
#[derive(Debug, Default, PartialEq)]
pub struct Replaced {
    /// The new copy, `(UIDVALIDITY, UID)`, when known.
    pub copy: Option<(u32, u32)>,
    /// Older copies deleted.
    pub deleted: Vec<u32>,
    /// Every older copy is known to be gone. False when a search had to be trusted and
    /// couldn't be (it didn't find the copy just made): older copies were kept, not risked.
    pub certain: bool,
}

/// Saves a draft: APPENDs it to `mailbox` (flagged \Draft and \Seen), then deletes the copy
/// the last save made, `previous` (`(UIDVALIDITY, UID)`), exactly. The new copy's UID comes
/// from APPENDUID (UIDPLUS). Only when it can't be known that way, or there's no previous copy
/// on record, or `maybe_strays` (an earlier upload was cut off and may have left a copy) does a
/// search by the draft's Message-ID stand in, and then only if it finds the copy just made.
pub async fn replace_draft(
    session: &mut async_imap::Session<impl Io>,
    caps: Caps,
    mailbox: &str,
    raw: &[u8],
    message_id: &str,
    previous: Option<(u32, u32)>,
    maybe_strays: bool,
) -> Result<Replaced> {
    let before = timed("SELECT", session.select(mailbox)).await??;
    let validity = before.uid_validity.unwrap_or(0);
    let appended = timed("APPEND", append_uid(session, mailbox, "(\\Draft \\Seen)", raw)).await??;
    let appended = appended.filter(|(v, _)| *v == validity || validity == 0);
    let validity = appended.map_or(validity, |(v, _)| v);
    let previous = previous.filter(|(v, _)| *v == validity && validity != 0).map(|(_, uid)| uid);
    let found = if appended.is_none() || previous.is_none() || maybe_strays {
        // Selected again, so the search sees the new copy.
        select(session, mailbox).await?;
        Some(uids_with_message_id(session, message_id).await?)
    } else {
        None
    };
    let plan = cleanup(appended.map(|(_, uid)| uid), before.uid_next, previous, found.as_deref());
    if !plan.certain {
        log::warn!("draft {message_id}: the search didn't find the copy just saved, so older copies were kept");
    }
    delete_uids(session, caps, &plan.delete).await?;
    Ok(Replaced { copy: plan.new.map(|uid| (validity, uid)), deleted: plan.delete, certain: plan.certain })
}

/// Which older copies a draft save deletes ([`replace_draft`]).
#[derive(Debug, PartialEq)]
struct Cleanup {
    new: Option<u32>,
    delete: Vec<u32>,
    certain: bool,
}

/// [`replace_draft`]'s decision. `appended`: the new copy's UID if the server said;
/// `uid_next`: the mailbox's UIDNEXT before the APPEND, so a copy at or above it is new;
/// `previous`: the last save's copy, in this UIDVALIDITY; `found`: the Message-ID search's
/// hits (headers checked), if it ran. UIDs only grow, so copies below the new one are older.
fn cleanup(appended: Option<u32>, uid_next: Option<u32>, previous: Option<u32>, found: Option<&[u32]>) -> Cleanup {
    let new = appended.or_else(|| found.zip(uid_next).and_then(|(f, next)| f.iter().copied().filter(|u| *u >= next).max()));
    // A search that doesn't find the copy just made can't be trusted to have found the others.
    let trusted = found.zip(new).is_some_and(|(f, n)| f.contains(&n));
    let mut delete: Vec<u32> = previous.filter(|p| Some(*p) != new).into_iter().collect();
    if let (true, Some(f), Some(n)) = (trusted, found, new) {
        delete.extend(f.iter().copied().filter(|u| *u < n));
    }
    delete.sort_unstable();
    delete.dedup();
    Cleanup { new, delete, certain: found.map_or(previous.is_some(), |_| trusted) }
}

/// APPENDs a message, returning its `(UIDVALIDITY, UID)` if the server reports it (APPENDUID,
/// from UIDPLUS). async-imap's own APPEND drops that.
async fn append_uid(session: &mut async_imap::Session<impl Io>, mailbox: &str, flags: &str, raw: &[u8]) -> Result<Option<(u32, u32)>> {
    use async_imap::imap_proto::{Response, ResponseCode, Status, UidSetMember};
    use tokio::io::AsyncWriteExt;
    let tag = session.run_command(format!("APPEND {} {flags} {{{}}}", quoted(mailbox), raw.len())).await?;
    // As async-imap reports a refusal, so errors read the same.
    fn refused(status: &Status, code: &impl std::fmt::Debug, info: &impl std::fmt::Debug) -> anyhow::Error {
        let what = format!("code: {code:?}, info: {info:?}");
        anyhow::Error::new(if *status == Status::Bad { async_imap::error::Error::Bad(what) } else { async_imap::error::Error::No(what) })
    }
    loop {
        let r = session.read_response().await.ok_or(async_imap::error::Error::ConnectionLost)??;
        match r.parsed() {
            Response::Continue { .. } => break,
            Response::Done { tag: t, status, code, information } if *t == tag => return Err(refused(status, code, information)),
            _ => {}
        }
    }
    let stream = session.as_mut();
    stream.write_all(raw).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await?;
    loop {
        let r = session.read_response().await.ok_or(async_imap::error::Error::ConnectionLost)??;
        if let Response::Done { tag: t, status, code, information } = r.parsed()
            && *t == tag
        {
            if *status != Status::Ok {
                return Err(refused(status, code, information));
            }
            return Ok(match code {
                Some(ResponseCode::AppendUid(validity, uids)) => match uids.as_slice() {
                    [UidSetMember::Uid(uid)] => Some((*validity, *uid)),
                    _ => None,
                },
                _ => None,
            });
        }
    }
}

/// Deletes a draft's copies from `mailbox`: the one on record, `known` (`(UIDVALIDITY, UID)`),
/// and, with `search`, any others the Message-ID search finds (headers checked). Returns their
/// UIDs.
pub async fn purge_message_id(
    session: &mut async_imap::Session<impl Io>,
    caps: Caps,
    mailbox: &str,
    message_id: &str,
    known: Option<(u32, u32)>,
    search: bool,
) -> Result<Vec<u32>> {
    let selected = timed("SELECT", session.select(mailbox)).await??;
    let validity = selected.uid_validity.unwrap_or(0);
    let mut uids = if search { uids_with_message_id(session, message_id).await? } else { Vec::new() };
    uids.extend(known.filter(|(v, _)| *v == validity && validity != 0).map(|(_, uid)| uid));
    uids.sort_unstable();
    uids.dedup();
    delete_uids(session, caps, &uids).await?;
    Ok(uids)
}

/// UIDs of the messages in the selected mailbox with exactly this Message-ID, not already
/// flagged \Deleted. SEARCH matches substrings, and some servers match loosely, so each hit's
/// header is checked: a draft's save must never delete anything else.
async fn uids_with_message_id(session: &mut async_imap::Session<impl Io>, message_id: &str) -> Result<Vec<u32>> {
    let query = format!("UNDELETED HEADER Message-ID {}", quoted(message_id));
    let hits = timed("SEARCH", session.uid_search(query)).await??;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let set = hits.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let fetched: Vec<Fetch> =
        timed("FETCH ids", async { session.uid_fetch(set, "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])").await?.try_collect::<Vec<_>>().await }).await??;
    Ok(fetched
        .iter()
        .filter(|f| f.header().and_then(|h| mime::parse(h, true)).is_some_and(|p| p.message_id == message_id))
        .filter_map(|f| f.uid)
        .filter(|u| hits.contains(u))
        .collect())
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
pub(crate) mod fake {
    //! A scripted IMAP server on an in-memory pipe, to check what Envoy sends.

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};

    /// Every command the client sent, without its tag (a literal's bytes are left out).
    pub type Log = Arc<Mutex<Vec<String>>>;

    /// A logged-in session with a server that answers each command with `answer(command)`:
    /// the response lines, where `TAG` stands for the command's tag. `None` answers
    /// `TAG OK done`.
    pub async fn session(answer: impl Fn(&str) -> Option<String> + Send + 'static) -> (async_imap::Session<DuplexStream>, Log) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let log: Log = Arc::default();
        let seen = log.clone();
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server);
            let mut r = BufReader::new(r);
            w.write_all(b"* OK fake ready\r\n").await.unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let Some((tag, command)) = line.trim_end().split_once(' ') else { return };
                let (tag, command) = (tag.to_owned(), command.to_owned());
                // A literal follows: ask for it and read it (plus the line's end).
                if let Some(n) = command.strip_suffix('}').and_then(|c| c.rsplit_once('{')).and_then(|(_, n)| n.parse::<usize>().ok()) {
                    w.write_all(b"+ go ahead\r\n").await.unwrap();
                    let mut literal = vec![0; n + 2];
                    r.read_exact(&mut literal).await.unwrap();
                }
                seen.lock().unwrap().push(command.clone());
                let reply = if command.starts_with("LOGIN") { None } else { answer(&command) };
                let reply = reply.unwrap_or_else(|| "TAG OK done\r\n".into()).replace("TAG", &tag);
                w.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let mut c = async_imap::Client::new(client);
        c.read_response().await.unwrap().unwrap();
        (c.login("me", "pw").await.map_err(|e| e.0).unwrap(), log)
    }

    /// A SELECT's answer.
    pub fn selected(validity: u32, uid_next: u32) -> String {
        format!("* 3 EXISTS\r\n* OK [UIDVALIDITY {validity}] ok\r\n* OK [UIDNEXT {uid_next}] ok\r\nTAG OK [READ-WRITE] done\r\n")
    }

    /// A UID FETCH of Message-ID headers' answer: `(uid, Message-ID)` each.
    pub fn message_ids(found: &[(u32, &str)]) -> String {
        let mut out = String::new();
        for (i, (uid, mid)) in found.iter().enumerate() {
            let header = format!("Message-ID: {mid}\r\n\r\n");
            out.push_str(&format!("* {} FETCH (UID {uid} BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{header})\r\n", i + 1, header.len()));
        }
        out + "TAG OK done\r\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(uidplus: bool) -> Caps {
        Caps { moves: false, idle: false, uidplus }
    }

    fn commands(log: &fake::Log) -> Vec<String> {
        log.lock().unwrap().iter().filter(|c| !c.starts_with("LOGIN")).cloned().collect()
    }

    fn expunges_everything(cmds: &[String]) -> bool {
        cmds.iter().any(|c| c.trim() == "EXPUNGE" || c.starts_with("EXPUNGE"))
    }

    #[tokio::test]
    async fn without_uidplus_deleting_only_flags_the_message() {
        let (mut s, log) = fake::session(|c| c.starts_with("SELECT").then(|| fake::selected(7, 10))).await;
        expunge(&mut s, caps(false), "Trash", 5, true).await.unwrap();
        move_message(&mut s, caps(false), "INBOX", 6, "Archive").await.unwrap();
        let cmds = commands(&log);
        assert!(!expunges_everything(&cmds), "a plain EXPUNGE would destroy everything else flagged \\Deleted: {cmds:?}");
        assert!(cmds.contains(&"UID STORE 5 +FLAGS.SILENT (\\Deleted)".to_owned()), "{cmds:?}");
        assert!(cmds.contains(&"UID COPY 6 \"Archive\"".to_owned()) && cmds.contains(&"UID STORE 6 +FLAGS.SILENT (\\Deleted)".to_owned()), "{cmds:?}");
    }

    #[tokio::test]
    async fn with_uidplus_only_the_given_messages_are_expunged() {
        let (mut s, log) = fake::session(|c| c.starts_with("SELECT").then(|| fake::selected(7, 10))).await;
        expunge(&mut s, caps(true), "Trash", 5, true).await.unwrap();
        let cmds = commands(&log);
        assert!(!expunges_everything(&cmds), "{cmds:?}");
        assert_eq!(cmds.last().map(String::as_str), Some("UID EXPUNGE 5"));
    }

    #[tokio::test]
    async fn without_uidplus_a_draft_save_never_expunges_the_mailbox() {
        let (mut s, log) = fake::session(|c| {
            if c.starts_with("SELECT") {
                Some(fake::selected(7, 21))
            } else if c.starts_with("UID SEARCH") {
                Some("* SEARCH 5 21\r\nTAG OK done\r\n".into())
            } else if c.starts_with("UID FETCH") {
                Some(fake::message_ids(&[(5, "<d@envoy>"), (21, "<d@envoy>")]))
            } else {
                None
            }
        })
        .await;
        let done = replace_draft(&mut s, caps(false), "Drafts", b"Subject: x\r\n\r\nhi\r\n", "<d@envoy>", None, false).await.unwrap();
        let cmds = commands(&log);
        assert!(!expunges_everything(&cmds), "{cmds:?}");
        assert!(cmds.contains(&"UID STORE 5 +FLAGS.SILENT (\\Deleted)".to_owned()), "the old copy is flagged: {cmds:?}");
        assert_eq!(done, Replaced { copy: Some((7, 21)), deleted: vec![5], certain: true }, "the new copy found above UIDNEXT");
    }

    #[test]
    fn failures_are_told_apart() {
        use async_imap::error::Error;
        let no = |s: &str| anyhow::Error::new(Error::No(s.into()));
        // Codes imap-proto doesn't know arrive in the text; ones it knows, in the code.
        assert_eq!(failure(&no("code: None, info: Some(\"[UNAVAILABLE] busy, try later\")")), Failure::Temporary);
        assert_eq!(failure(&no("code: None, info: Some(\"[OVERQUOTA] mailbox full\")")), Failure::Permanent);
        assert_eq!(failure(&no("code: Some(TryCreate), info: None")), Failure::Permanent);
        assert_eq!(failure(&no("code: None, info: Some(\"APPEND failed\")")), Failure::Unknown);
        assert_eq!(failure(&anyhow::Error::new(Error::Bad("parse error".into()))), Failure::Permanent);
        assert_eq!(failure(&anyhow!("FETCH timed out (connection lost?)")), Failure::Temporary);
        assert_eq!(failure(&anyhow::Error::new(Refused("account address is invalid".into()))), Failure::Permanent);
    }

    #[test]
    fn a_save_deletes_the_copy_it_knows_exactly() {
        assert_eq!(cleanup(Some(21), Some(21), Some(5), None), Cleanup { new: Some(21), delete: vec![5], certain: true });
    }

    #[test]
    fn a_search_that_misses_the_new_copy_is_not_trusted() {
        // APPENDUID says 21, the search doesn't have it: nothing it found is deleted.
        assert_eq!(cleanup(Some(21), Some(21), None, Some(&[5, 9])), Cleanup { new: Some(21), delete: vec![], certain: false });
        // No APPENDUID, and nothing found above UIDNEXT: the new copy can't be told apart.
        assert_eq!(cleanup(None, Some(21), None, Some(&[5, 9])), Cleanup { new: None, delete: vec![], certain: false });
        assert_eq!(cleanup(None, None, None, Some(&[5, 9])), Cleanup { new: None, delete: vec![], certain: false });
        // The copy on record still goes: it's this draft's, and the APPEND succeeded.
        assert_eq!(cleanup(None, Some(21), Some(5), Some(&[9])), Cleanup { new: None, delete: vec![5], certain: false });
    }

    #[test]
    fn a_trusted_search_deletes_only_older_copies() {
        // 30 came after the copy just made (another computer): kept.
        assert_eq!(cleanup(Some(21), Some(21), None, Some(&[5, 21, 30])), Cleanup { new: Some(21), delete: vec![5], certain: true });
        assert_eq!(cleanup(None, Some(21), None, Some(&[5, 21])), Cleanup { new: Some(21), delete: vec![5], certain: true });
        // A copy left by an interrupted upload, besides the one on record.
        assert_eq!(cleanup(Some(21), Some(21), Some(5), Some(&[5, 8, 21])), Cleanup { new: Some(21), delete: vec![5, 8], certain: true });
    }

    fn drafts_server(search: &'static str, found: &'static [(u32, &'static str)]) -> impl Fn(&str) -> Option<String> + Send + 'static {
        move |c: &str| {
            if c.starts_with("SELECT") {
                Some(fake::selected(7, 21))
            } else if c.starts_with("APPEND") {
                Some("TAG OK [APPENDUID 7 21] saved\r\n".into())
            } else if c.starts_with("UID SEARCH") {
                Some(format!("* SEARCH {search}\r\nTAG OK done\r\n"))
            } else if c.starts_with("UID FETCH") {
                Some(fake::message_ids(found))
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn with_appenduid_the_previous_copy_goes_by_uid_without_a_search() {
        let (mut s, log) = fake::session(drafts_server("", &[])).await;
        let done = replace_draft(&mut s, caps(true), "Drafts", b"Subject: x\r\n\r\nhi\r\n", "<d@envoy>", Some((7, 5)), false).await.unwrap();
        assert_eq!(done, Replaced { copy: Some((7, 21)), deleted: vec![5], certain: true });
        let cmds = commands(&log);
        assert!(!cmds.iter().any(|c| c.starts_with("UID SEARCH")), "{cmds:?}");
        assert_eq!(&cmds[cmds.len() - 2..], ["UID STORE 5 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 5"]);
    }

    #[tokio::test]
    async fn a_search_that_misses_the_new_copy_deletes_nothing() {
        let (mut s, log) = fake::session(drafts_server("5 9", &[(5, "<d@envoy>"), (9, "<d@envoy>")])).await;
        let done = replace_draft(&mut s, caps(true), "Drafts", b"Subject: x\r\n\r\nhi\r\n", "<d@envoy>", None, false).await.unwrap();
        assert_eq!(done, Replaced { copy: Some((7, 21)), deleted: vec![], certain: false });
        let cmds = commands(&log);
        assert!(!cmds.iter().any(|c| c.starts_with("UID STORE") || c.contains("EXPUNGE")), "{cmds:?}");
    }

    #[tokio::test]
    async fn a_uid_from_another_uidvalidity_is_not_trusted() {
        let (mut s, log) = fake::session(drafts_server("5 21", &[(5, "<d@envoy>"), (21, "<d@envoy>")])).await;
        // The mailbox was recreated (UIDVALIDITY 7, not 6): UID 4 means nothing now; the search decides.
        let done = replace_draft(&mut s, caps(true), "Drafts", b"Subject: x\r\n\r\nhi\r\n", "<d@envoy>", Some((6, 4)), false).await.unwrap();
        assert_eq!(done.deleted, vec![5]);
        assert!(!commands(&log).iter().any(|c| c.contains("STORE 4 ")));
    }

    #[tokio::test]
    async fn a_purge_deletes_the_known_copy_and_any_found() {
        let (mut s, log) = fake::session(drafts_server("8", &[(8, "<d@envoy>")])).await;
        assert_eq!(purge_message_id(&mut s, caps(true), "Drafts", "<d@envoy>", Some((7, 5)), true).await.unwrap(), vec![5, 8]);
        assert_eq!(commands(&log).last().map(String::as_str), Some("UID EXPUNGE 5,8"));
    }

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
