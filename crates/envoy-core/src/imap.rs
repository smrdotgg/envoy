//! IMAP: connecting, listing mailboxes, syncing headers, flags and bodies, and replaying
//! queued changes.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_imap::types::Flag;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::accounts::Account;
use crate::mime;
use crate::model::{Role, flags};
use crate::snooze;
use crate::store::Store;

mod wire;

pub use wire::{Fetched, Lost, ServerFlags};

pub type Stream = TlsStream<TcpStream>;
pub type Session = Connection<Stream>;

/// A logged-in IMAP connection. Once an exchange on it doesn't finish (the connection closed
/// or broke, a BYE, a timeout, a garbled answer), it's poisoned: what's left of that answer
/// could be read as the next command's, so every later command fails as a lost connection,
/// without being sent, and its owner connects again.
pub struct Connection<T: Io> {
    inner: async_imap::Session<T>,
    poisoned: Option<String>,
}

impl<T: Io> Connection<T> {
    pub fn new(inner: async_imap::Session<T>) -> Connection<T> {
        Connection { inner, poisoned: None }
    }

    /// Ok while every exchange so far finished.
    fn usable(&self) -> Result<()> {
        match &self.poisoned {
            Some(why) => Err(Lost(format!("an earlier command didn't finish ({why})")).into()),
            None => Ok(()),
        }
    }

    /// Passes `r` on, poisoning the connection if it says the exchange didn't finish.
    fn finished<R>(&mut self, r: Result<R>) -> Result<R> {
        if let Err(e) = &r
            && is_lost(e)
            && self.poisoned.is_none()
        {
            self.poisoned = Some(format!("{e:#}"));
        }
        r
    }

    pub async fn logout(&mut self) -> Result<()> {
        self.usable()?;
        let r = timed("LOGOUT", self.inner.logout()).await.and_then(|r| r.map_err(lost_if_transport));
        self.finished(r)
    }
}

/// async-imap's error as Envoy's: one of the connection's (I/O, closed, garbled) is [`Lost`];
/// the server's refusals stay as they are.
fn lost_if_transport(e: async_imap::error::Error) -> anyhow::Error {
    use async_imap::error::Error;
    match e {
        Error::Io(_) | Error::ConnectionLost | Error::Parse(_) => anyhow::Error::new(Lost(format!("{e}"))),
        e => anyhow::Error::new(e),
    }
}

/// The exchange didn't finish because the connection failed (closed, broke, timed out, or
/// sent what can't be parsed), not because the server refused: the session can't be trusted
/// any more, and what was asked may be asked again on a new one. Told by the error's type,
/// never its message.
pub fn is_lost(e: &anyhow::Error) -> bool {
    use async_imap::error::Error;
    e.chain().any(|c| {
        c.is::<Lost>() || c.is::<std::io::Error>() || matches!(c.downcast_ref::<Error>(), Some(Error::Io(_) | Error::ConnectionLost | Error::Parse(_)))
    })
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Short commands: a dead connection (laptop slept, Wi-Fi changed) is noticed quickly.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Bulk downloads, which can legitimately take a while on a slow link.
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);
/// A [`probe`]: a connection that still works answers a NOOP in a round trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the connection still answers: a NOOP, within a few seconds. One dropped without a
/// word (the computer slept, a NAT forgot it) never answers, and is found out here instead of
/// by the next real command's 30 s wait. Failing, the connection is done with.
pub async fn probe(session: &mut Connection<impl Io>) -> Result<()> {
    wire::ok_within(session, "NOOP", "NOOP", PROBE_TIMEOUT).await
}

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
    tokio::time::timeout(COMMAND_TIMEOUT, f).await.map_err(|_| anyhow::Error::new(Lost(format!("{what} timed out"))))
}

struct XOAuth2(String);

impl async_imap::Authenticator for &XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        self.0.clone()
    }
}

pub async fn connect(account: &Account, cred: &Credential) -> Result<Session> {
    connect_session(account, cred).await.map(Connection::new)
}

async fn connect_session(account: &Account, cred: &Credential) -> Result<async_imap::Session<Stream>> {
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
    /// It has the `\Snoozed` attribute: the server's own snoozed mailbox (RFC 9979), shown and
    /// never written into.
    pub native_snoozed: bool,
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

pub async fn list_mailboxes(session: &mut Connection<impl Io>) -> Result<Vec<ListedMailbox>> {
    let names = wire::list(session).await?;
    let mut out = Vec::new();
    for n in names {
        let mut role = Role::Other;
        let mut selectable = true;
        let native_snoozed = n.attributes.iter().any(|a| matches!(a, wire::Attribute::Other(s) if s == "\\snoozed"));
        for a in &n.attributes {
            match a {
                wire::Attribute::NoSelect => selectable = false,
                wire::Attribute::All => role = Role::All,
                wire::Attribute::Archive => role = Role::Archive,
                wire::Attribute::Drafts => role = Role::Drafts,
                wire::Attribute::Flagged => role = Role::Flagged,
                wire::Attribute::Junk => role = Role::Junk,
                wire::Attribute::Sent => role = Role::Sent,
                wire::Attribute::Trash => role = Role::Trash,
                wire::Attribute::Other(s) => {
                    if s == "\\important" {
                        role = Role::Important;
                    } else if s == "\\noselect" || s == "\\nonexistent" {
                        selectable = false;
                    }
                }
            }
        }
        let name = n.name;
        if name.eq_ignore_ascii_case("INBOX") {
            role = Role::Inbox;
        } else if role == Role::Other {
            role = role_from_name(&name);
        }
        let delimiter = n.delimiter.unwrap_or_else(|| "/".to_owned());
        let decoded = decode_utf7(&name);
        let leaf = decoded.strip_prefix("[Gmail]/").or_else(|| decoded.strip_prefix("[Google Mail]/")).unwrap_or(&decoded);
        let display = if role != Role::Other && role != Role::Important { role.label().to_owned() } else { leaf.rsplit(delimiter.as_str()).next().unwrap_or(leaf).to_owned() };
        // Gmail's "[Gmail]" parent is only a container.
        if name == "[Gmail]" || name == "[Google Mail]" {
            selectable = false;
        }
        out.push(ListedMailbox { name, display, role, delimiter, selectable, native_snoozed });
    }
    // Only one mailbox per role (the first): a server may mark two as \Sent. Snoozed is many.
    let mut seen = HashSet::new();
    for m in out.iter_mut() {
        if m.role != Role::Other && m.role != Role::Snoozed && !seen.insert(m.role) {
            m.role = Role::Other;
        }
    }
    mark_snoozed(&mut out);
    Ok(out)
}

/// Where this account's snooze buckets go, from its mailbox list ([`snooze::choose_parent`]).
pub fn snooze_parent(listed: &[ListedMailbox]) -> snooze::Parent {
    let names: Vec<snooze::Listed> = listed.iter().map(|m| snooze::Listed { name: &m.name, delimiter: &m.delimiter, native: m.native_snoozed }).collect();
    snooze::choose_parent(&names)
}

/// Gives the snooze buckets (a stamp under either parent in the personal namespace, by the
/// server's delimiter: [`snooze::bucket_wake`]), the parents, and the server's own snoozed
/// mailbox (by its `\Snoozed` attribute) their role, whatever they'd have had: they're shown
/// together as Snoozed, not as folders.
fn mark_snoozed(out: &mut [ListedMailbox]) {
    let parent = snooze_parent(out);
    let parents = snooze::parents(snooze::prefix_of(&parent.name));
    for m in out.iter_mut() {
        if m.native_snoozed || parents.contains(&m.name) || snooze::bucket_wake(&m.name, &parents, &parent.delimiter).is_some() {
            m.role = Role::Snoozed;
            m.display = Role::Snoozed.label().to_owned();
        }
    }
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
            Flag::Custom(k) if k.eq_ignore_ascii_case(snooze::NEW_KEYWORD) => flags::WOKEN,
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
    if bits & flags::WOKEN != 0 {
        v.push(snooze::NEW_KEYWORD);
    }
    v.join(" ")
}

/// A new message's flags, size, and the headers the list, recipient suggestions and the split
/// inbox read: the envelope, Bcc (in copies of mail you sent), and whether it's bulk or automated
/// ([`mime::is_bulk`], [`mime::BULK_HEADERS`]): a few short lines.
const HEADER_QUERY: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO CC BCC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-POST PRECEDENCE AUTO-SUBMITTED X-GITHUB-REASON X-GITLAB-PROJECT X-BUGZILLA-REASON X-JIRA-FINGERPRINT)])";

fn store_header(store: &Store, account: &str, mailbox: &str, f: &Fetched) -> Result<Option<crate::model::MessageId>> {
    let (Some(uid), Some(header)) = (f.uid, f.header.as_deref()) else { return Ok(None) };
    let Some(parsed) = mime::parse(header, true) else { return Ok(None) };
    Ok(Some(store.upsert_header(account, mailbox, uid, f.flags.unwrap_or(0), f.size.unwrap_or(0), f.internal_date.unwrap_or(0), &parsed)?))
}

pub struct SyncOutcome {
    pub changed: bool,
    pub new_messages: u32,
    /// The mailbox as the sync found it, for [`changed_since`].
    pub contents: Contents,
}

/// What a SELECT tells of a mailbox's contents: when it differs, mail came or went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contents {
    pub exists: u32,
    pub uid_next: Option<u32>,
    /// The mailbox's generation: another, and it was recreated (or reset), whatever the rest.
    pub uid_validity: Option<u32>,
}

impl Contents {
    fn of(mb: &wire::Selected) -> Contents {
        Contents { exists: mb.exists, uid_next: mb.uid_next, uid_validity: Some(mb.uid_validity) }
    }

    /// Whether `now` tells of mail come or gone since these: a message in (from anywhere)
    /// moves UIDNEXT on, one out changes the count, and a mailbox made again has another
    /// UIDVALIDITY. UIDNEXT or UIDVALIDITY missing on either side isn't compared.
    pub fn moved_on(self, now: Contents) -> bool {
        let differ = |a: Option<u32>, b: Option<u32>| matches!((a, b), (Some(a), Some(b)) if a != b);
        self.exists != now.exists || differ(self.uid_next, now.uid_next) || differ(self.uid_validity, now.uid_validity)
    }
}

/// Whether the server's folder list names `mailbox`: after it failed to open, whether it's gone.
pub async fn listed(session: &mut Connection<impl Io>, mailbox: &str) -> Result<bool> {
    wire::listed(session, mailbox).await
}

/// `mailbox`'s contents as a STATUS tells them, without selecting it: for
/// [`Contents::moved_on`] against what its last sync found.
pub async fn contents(session: &mut Connection<impl Io>, mailbox: &str) -> Result<Contents> {
    let told = wire::status(session, mailbox).await?;
    Ok(Contents { exists: told.messages, uid_next: told.uid_next, uid_validity: told.uid_validity })
}

/// Selects `mailbox` and says whether its contents changed since `synced` (what its last sync
/// found): mail delivered after that sync, which an IDLE started now would never report.
///
/// What the server said unasked before this is dropped first: it's older than the SELECT, which
/// counts it, and the channel async-imap files it in holds only so much, dropping what comes
/// once it's full, so [`idle`] would never hear of mail that arrives as it starts.
pub async fn changed_since(session: &mut Connection<impl Io>, mailbox: &str, synced: Option<Contents>) -> Result<bool> {
    news(session);
    let mb = wire::select(session, mailbox).await?;
    // A message expunged as the SELECT answered may be newer than its counts.
    Ok(synced != Some(Contents::of(&mb)) || mb.expunged)
}

/// Takes what the server said unasked (async-imap files it away) and says whether mail came or
/// went: EXISTS, RECENT or EXPUNGE.
fn news(session: &mut Connection<impl Io>) -> bool {
    news_in(|| session.inner.unsolicited_responses.try_recv().ok())
}

/// [`news`], from what `next` takes off async-imap's channel until it's empty.
fn news_in(mut next: impl FnMut() -> Option<async_imap::types::UnsolicitedResponse>) -> bool {
    use async_imap::types::UnsolicitedResponse as U;
    let mut news = false;
    while let Some(r) = next() {
        news |= matches!(r, U::Exists(_) | U::Recent(_) | U::Expunge(_));
    }
    news
}

/// Brings one mailbox's local copy up to date: new messages' headers, flag changes and
/// deletions, keeping the newest `window` messages.
/// Headers per FETCH during a first download.
const HEADER_CHUNK: u32 = 200;

fn store_fetched(store: &Mutex<Store>, account: &str, mailbox: &str, fetched: &[Fetched], above_uid: u32) -> Result<u32> {
    if fetched.is_empty() {
        return Ok(0);
    }
    let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
    st.transaction(|st| {
        let mut n = 0;
        // Messages this Envoy woke from a snooze come back pinned, wherever the server can't
        // keep `$new` for them.
        let woken = st.has_woken(account)? && st.role_of(account, mailbox) == Role::Inbox;
        for f in fetched {
            if !f.uid.is_some_and(|u| u > above_uid) {
                continue;
            }
            if let Some(id) = store_header(st, account, mailbox, f)? {
                n += 1;
                if woken {
                    st.apply_woken(id)?;
                }
            }
        }
        // Messages a queued move or expunge is taking away are hidden as they land, before
        // anyone sees them (after a reset, they come back as new rows).
        if n > 0 {
            st.rehide_queued(account, mailbox)?;
        }
        Ok(n)
    })
}

/// Brings one mailbox's local copy up to date (see the module docs). `progress` runs after
/// each chunk of a first download lands, so the UI can show it.
pub async fn sync_mailbox(session: &mut Connection<impl Io>, store: &Mutex<Store>, account: &str, mailbox: &str, window: u32, progress: &(dyn Fn() + Send + Sync)) -> Result<SyncOutcome> {
    let mb = wire::select(session, mailbox).await?;
    let contents = Contents::of(&mb);
    let uidvalidity = mb.uid_validity;
    let mut changed = false;
    let mut new_messages = 0;
    let (local, max_uid) = {
        let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
        // A new generation: the cached UIDs mean nothing now. Messages queued ops are taking
        // away are hidden again as the new one downloads (store_fetched).
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
        return Ok(SyncOutcome { changed, new_messages, contents });
    }

    if local.is_empty() {
        // First download: newest first, a chunk at a time, so the list fills in as it goes,
        // until the window holds `window` messages to show. Ones flagged \Deleted (removed
        // elsewhere, not expunged yet) are kept, hidden, but don't count.
        let mut wanted = window.max(1);
        let mut hi = mb.exists;
        while hi >= 1 && wanted > 0 {
            let lo = hi.saturating_sub(HEADER_CHUNK.min(wanted) - 1).max(1);
            let fetched = wire::fetch(session, false, &format!("{lo}:{hi}"), HEADER_QUERY, "FETCH headers").await?;
            // Messages, not the flag updates a server may send in among them (another client's
            // changes, as FETCH responses without a header).
            let shown = fetched.iter().filter(|f| f.uid.is_some() && f.header.is_some() && f.flags.unwrap_or(0) & flags::DELETED == 0).count() as u32;
            wanted = wanted.saturating_sub(shown);
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
        let fetched = wire::fetch(session, true, &format!("{}:*", max_uid + 1), HEADER_QUERY, "FETCH new").await?;
        let n = store_fetched(store, account, mailbox, &fetched, max_uid)?;
        if n > 0 {
            new_messages += n;
            changed = true;
        }
    }

    // Flags and deletions for what we already have.
    if let Some(&(min_uid, _)) = local.first() {
        let server = wire::uid_flags(session, min_uid, max_uid).await?;
        // Where the server can't keep `$new`, a woken message's mark is this computer's.
        changed |= store.lock().map_err(|_| anyhow!("store poisoned"))?.reconcile(account, mailbox, &local, &server, window, !mb.keywords)?;
    }
    store.lock().map_err(|_| anyhow!("store poisoned"))?.mark_synced(account, mailbox)?;
    Ok(SyncOutcome { changed, new_messages, contents })
}

/// Unread and total counts for the sidebar.
pub async fn status(session: &mut Connection<impl Io>, mailbox: &str) -> Result<(u32, u32)> {
    let told = wire::status(session, mailbox).await?;
    Ok((told.unseen, told.messages))
}

/// Downloads the bodies of `uids` in `mailbox`, UIDs from generation `validity` (UIDVALIDITY)
/// of it as cached, and stores each against the cached message its UID names. Only when the
/// server, the cache and the request all have that generation: in another, a UID may name
/// another message, so nothing is stored (nothing is fetched, if the server has moved on), and
/// the error is [`NewGeneration`]: the mailbox wants syncing again. Returns the messages that
/// got their bodies.
pub async fn fetch_bodies(session: &mut Connection<impl Io>, store: &Mutex<Store>, account: &str, mailbox: &str, validity: u32, uids: &[u32]) -> Result<Vec<i64>> {
    if uids.is_empty() {
        return Ok(vec![]);
    }
    let new_generation = |now: u32| anyhow::Error::new(NewGeneration { mailbox: mailbox.to_owned(), validity: now, found: None });
    let selected = wire::select(session, mailbox).await?;
    if validity == 0 || selected.uid_validity != validity {
        return Err(new_generation(selected.uid_validity));
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let fetched = wire::fetch(session, true, &set, "(UID BODY.PEEK[])", "FETCH bodies").await?;
    let mut ids = Vec::new();
    let st = store.lock().map_err(|_| anyhow!("store poisoned"))?;
    // The cache may have moved on while they came (another connection saw a reset).
    let cached = st.uidvalidity(account, mailbox)?;
    if cached != validity {
        return Err(new_generation(cached));
    }
    for f in &fetched {
        let (Some(uid), Some(raw)) = (f.uid, f.body.as_deref()) else { continue };
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

pub async fn select(session: &mut Connection<impl Io>, mailbox: &str) -> Result<()> {
    wire::select(session, mailbox).await?;
    Ok(())
}

/// The message a queued op acts on: UID `uid` in generation `validity` (the UIDVALIDITY) of its
/// mailbox, and its Message-ID, to find it again in another generation.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub uid: u32,
    pub validity: u32,
    pub message_id: &'a str,
}

/// A queued op's UID (or a download's) is from another generation of its mailbox than the
/// server has now (its UIDVALIDITY changed: the mailbox was recreated, or reset), so it may name
/// a different message, or none: the op didn't run. `found`: the message's UID in this generation, when its
/// Message-ID finds exactly one.
#[derive(Debug, PartialEq)]
pub struct NewGeneration {
    pub mailbox: String,
    pub validity: u32,
    pub found: Option<u32>,
}

impl std::fmt::Display for NewGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is a new generation (UIDVALIDITY {}) since the change was queued", self.mailbox, self.validity)
    }
}

impl std::error::Error for NewGeneration {}

/// SELECTs `mailbox` for an op on `target`: Ok only when the mailbox is still in the target's
/// generation. Otherwise nothing is sent with the target's UID, and the error
/// ([`NewGeneration`]) says where its message is now, if its Message-ID finds it once. An
/// unknown generation (0) is never taken for the current one.
async fn select_target(session: &mut Connection<impl Io>, mailbox: &str, target: &Target<'_>) -> Result<wire::Selected> {
    let selected = wire::select(session, mailbox).await?;
    if target.validity != 0 && selected.uid_validity == target.validity {
        return Ok(selected);
    }
    let found = match target.message_id {
        "" => None,
        message_id => match uids_with_message_id(session, message_id).await?.as_slice() {
            [uid] => Some(*uid),
            _ => None,
        },
    };
    Err(NewGeneration { mailbox: mailbox.to_owned(), validity: selected.uid_validity, found }.into())
}

pub async fn store_flags(session: &mut Connection<impl Io>, mailbox: &str, target: &Target<'_>, add: u32, remove: u32) -> Result<()> {
    let selected = select_target(session, mailbox, target).await?;
    // `$new` goes to the server only where it keeps keywords; elsewhere it's this computer's.
    let keep = if selected.keywords { u32::MAX } else { !flags::WOKEN };
    let (add, remove) = (add & keep, remove & keep);
    let uid = target.uid;
    if add != 0 {
        wire::ok(session, &format!("UID STORE {uid} +FLAGS.SILENT ({})", flag_names(add)), "STORE").await?;
    }
    if remove != 0 {
        wire::ok(session, &format!("UID STORE {uid} -FLAGS.SILENT ({})", flag_names(remove)), "STORE").await?;
    }
    Ok(())
}

pub async fn move_message(session: &mut Connection<impl Io>, caps: Caps, mailbox: &str, target: &Target<'_>, dest: &str) -> Result<()> {
    select_target(session, mailbox, target).await?;
    let uid = target.uid;
    if caps.moves {
        wire::ok(session, &format!("UID MOVE {uid} {}", quoted(dest)), "MOVE").await?;
    } else {
        wire::ok(session, &format!("UID COPY {uid} {}", quoted(dest)), "COPY").await?;
        delete_uids(session, caps, &[uid]).await?;
    }
    Ok(())
}

/// Copies a message to `dest`, leaving it where it is: on Gmail, adds the label `dest`.
pub async fn copy_message(session: &mut Connection<impl Io>, mailbox: &str, target: &Target<'_>, dest: &str) -> Result<()> {
    select_target(session, mailbox, target).await?;
    let uid = target.uid;
    wire::ok(session, &format!("UID COPY {uid} {}", quoted(dest)), "COPY").await
}

pub async fn expunge(session: &mut Connection<impl Io>, caps: Caps, mailbox: &str, target: &Target<'_>) -> Result<()> {
    select_target(session, mailbox, target).await?;
    delete_uids(session, caps, &[target.uid]).await
}

/// Deletes these messages from the selected mailbox: flags them \Deleted and, with UIDPLUS,
/// expunges exactly them (UID EXPUNGE). Without UIDPLUS they stay flagged, for the server or
/// the user to remove: a plain EXPUNGE would also destroy anything else flagged \Deleted there,
/// by the user or another client, so Envoy never sends one. Envoy's lists hide \Deleted mail.
async fn delete_uids(session: &mut Connection<impl Io>, caps: Caps, uids: &[u32]) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    wire::ok(session, &format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"), "STORE").await?;
    if caps.uidplus {
        wire::ok(session, &format!("UID EXPUNGE {set}"), "EXPUNGE").await?;
    }
    Ok(())
}

// ── Snoozes ([`crate::snooze`]) ──

/// A mailbox Envoy needs couldn't be created: the server refused, and doesn't list it either.
#[derive(Debug)]
pub struct CantCreate {
    pub mailbox: String,
    pub why: String,
}

impl std::fmt::Display for CantCreate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "couldn't create {} ({})", self.mailbox, self.why)
    }
}

impl std::error::Error for CantCreate {}

/// Creates `name`, or finds it there already: success either way. A refusal is taken for "it's
/// there" only when LIST names it (servers say so with ALREADYEXISTS, or with nothing at all).
pub async fn create_mailbox(session: &mut Connection<impl Io>, name: &str) -> Result<()> {
    match wire::ok(session, &format!("CREATE {}", quoted(name)), "CREATE").await {
        Ok(()) => Ok(()),
        Err(e) if is_lost(&e) => Err(e),
        Err(e) if wire::listed(session, name).await? => {
            log::debug!("CREATE {name}: {e:#}, and it's there");
            Ok(())
        }
        Err(e) => Err(CantCreate { mailbox: name.to_owned(), why: format!("{e:#}") }.into()),
    }
}

/// Subscribes to `name`, so other clients that show only subscribed folders show it. A refusal
/// changes nothing (a server without subscriptions); a lost connection is still one.
async fn subscribe(session: &mut Connection<impl Io>, name: &str, on: bool) -> Result<()> {
    let verb = if on { "SUBSCRIBE" } else { "UNSUBSCRIBE" };
    match wire::ok(session, &format!("{verb} {}", quoted(name)), verb).await {
        Err(e) if is_lost(&e) => Err(e),
        Err(e) => {
            log::debug!("{verb} {name}: {e:#}");
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// Makes sure snooze bucket `bucket` exists under `parent`, both subscribed. [`CantCreate`]
/// when the server won't have either.
pub async fn ensure_bucket(session: &mut Connection<impl Io>, parent: &str, bucket: &str) -> Result<()> {
    create_mailbox(session, parent).await?;
    create_mailbox(session, bucket).await?;
    subscribe(session, parent, true).await?;
    subscribe(session, bucket, true).await
}

/// What waking a bucket did ([`wake_bucket`]).
#[derive(Debug, Default, PartialEq)]
pub struct Woken {
    /// How many messages went back to the inbox.
    pub moved: usize,
    /// Their Message-IDs (the ones they have), to know them by there.
    pub message_ids: Vec<String>,
}

/// Wakes snooze bucket `bucket`: every message in it not flagged \Deleted goes back to `inbox`,
/// unread, with `$new` where the server keeps keywords. With MOVE, one UID MOVE; without, a
/// COPY, then the originals deleted (expunged only with UIDPLUS: Envoy never sends a plain
/// EXPUNGE). Nothing in the inbox is ever deleted: should two Envoys copy the same messages at
/// once (each waits at random first, so it's rare), the inbox has two copies, and the list shows
/// them as one. The same call on an emptied bucket (by another Envoy, or this one) sends nothing
/// more than a SELECT. The messages are named by the UIDs a search just found, never `1:*`.
pub async fn wake_bucket(session: &mut Connection<impl Io>, caps: Caps, bucket: &str, inbox: &str) -> Result<Woken> {
    let selected = wire::select(session, bucket).await?;
    if selected.exists == 0 {
        return Ok(Woken::default());
    }
    let mut uids: Vec<u32> = wire::uid_search(session, "UNDELETED").await?.into_iter().collect();
    if uids.is_empty() {
        return Ok(Woken::default());
    }
    uids.sort_unstable();
    let set = uid_set(&uids);
    let message_ids = message_ids_of(session, &set).await?;
    wire::ok(session, &format!("UID STORE {set} -FLAGS.SILENT (\\Seen)"), "STORE").await?;
    if selected.keywords {
        wire::ok(session, &format!("UID STORE {set} +FLAGS.SILENT ({})", snooze::NEW_KEYWORD), "STORE").await?;
    }
    if caps.moves {
        wire::ok(session, &format!("UID MOVE {set} {}", quoted(inbox)), "MOVE").await?;
    } else {
        wire::ok(session, &format!("UID COPY {set} {}", quoted(inbox)), "COPY").await?;
        delete_uids(session, caps, &uids).await?;
    }
    Ok(Woken { moved: uids.len(), message_ids })
}

/// The Message-IDs of the selected mailbox's messages `set`, as their headers say.
async fn message_ids_of(session: &mut Connection<impl Io>, set: &str) -> Result<Vec<String>> {
    let fetched = wire::fetch(session, true, set, "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])", "FETCH ids").await?;
    Ok(fetched.iter().filter_map(|f| f.header.as_deref().and_then(|h| mime::parse(h, true))).map(|p| p.message_id).filter(|m| !m.is_empty()).collect())
}

/// Deletes snooze bucket `bucket` if the server says it's empty right before (and unsubscribes
/// it). Only for Gmail, where DELETE removes a label and never mail, and a week past the bucket's
/// time ([`snooze::old_enough`]): the caller sees to both. True when it's gone, deleted here or
/// found gone already.
pub async fn cleanup_bucket(session: &mut Connection<impl Io>, bucket: &str, inbox: &str) -> Result<bool> {
    // Never the selected mailbox; selected first, so the STATUS is right before the DELETE.
    wire::select(session, inbox).await?;
    let messages = match wire::status(session, bucket).await {
        Ok(told) => told.messages,
        Err(e) if is_lost(&e) => return Err(e),
        // Gone already (another Envoy, or the user).
        Err(_) if !wire::listed(session, bucket).await? => return Ok(true),
        Err(e) => return Err(e),
    };
    if messages != 0 {
        return Ok(false);
    }
    match wire::ok(session, &format!("DELETE {}", quoted(bucket)), "DELETE").await {
        Ok(()) => {}
        Err(e) if is_lost(&e) => return Err(e),
        Err(_) if !wire::listed(session, bucket).await? => {}
        Err(e) => return Err(e),
    }
    subscribe(session, bucket, false).await?;
    Ok(true)
}

/// A UID set, runs folded: `3:5,8`.
fn uid_set(sorted: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        while i + 1 < sorted.len() && sorted[i + 1] == sorted[i] + 1 {
            i += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if sorted[i] == start { out.push_str(&start.to_string()) } else { out.push_str(&format!("{start}:{}", sorted[i])) }
        i += 1;
    }
    out
}

pub async fn append(session: &mut Connection<impl Io>, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
    append_uid(session, mailbox, if seen { "(\\Seen)" } else { "" }, raw).await?;
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
    session: &mut Connection<impl Io>,
    caps: Caps,
    mailbox: &str,
    raw: &[u8],
    message_id: &str,
    previous: Option<(u32, u32)>,
    maybe_strays: bool,
) -> Result<Replaced> {
    let before = wire::select(session, mailbox).await?;
    let validity = before.uid_validity;
    let appended = append_uid(session, mailbox, "(\\Draft \\Seen)", raw).await?;
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
/// from UIDPLUS). async-imap's own APPEND drops that. Read to its tagged status, as
/// [`wire::run`] reads the others: anything short of that is a lost connection, and poisons it.
async fn append_uid(session: &mut Connection<impl Io>, mailbox: &str, flags: &str, raw: &[u8]) -> Result<Option<(u32, u32)>> {
    use async_imap::imap_proto::{Response, ResponseCode, Status, UidSetMember};
    use tokio::io::AsyncWriteExt;
    use wire::refused;
    session.usable()?;
    let flags = if flags.is_empty() { String::new() } else { format!(" {flags}") };
    let command = format!("APPEND {}{flags} {{{}}}", quoted(mailbox), raw.len());
    let inner = &mut session.inner;
    let exchange = async {
        let lost = |why: &str| anyhow::Error::new(Lost(format!("APPEND: {why}")));
        let tag = inner.run_command(command).await.map_err(lost_if_transport)?;
        loop {
            let r = inner.read_response().await.ok_or_else(|| lost("the connection closed before the server finished"))?.map_err(|e| lost(&e.to_string()))?;
            match r.parsed() {
                Response::Continue { .. } => break,
                Response::Done { tag: t, status, code, information } if *t == tag => return Err(refused(status, code, information)),
                Response::Data { status: Status::Bye, .. } => return Err(lost("the server said BYE")),
                _ => {}
            }
        }
        let stream = inner.as_mut();
        stream.write_all(raw).await.map_err(|e| lost(&e.to_string()))?;
        stream.write_all(b"\r\n").await.map_err(|e| lost(&e.to_string()))?;
        stream.flush().await.map_err(|e| lost(&e.to_string()))?;
        loop {
            let r = inner.read_response().await.ok_or_else(|| lost("the connection closed before the server finished"))?.map_err(|e| lost(&e.to_string()))?;
            if let Response::Data { status: Status::Bye, .. } = r.parsed() {
                return Err(lost("the server said BYE"));
            }
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
    };
    // A big message on a slow link takes a while.
    let r = tokio::time::timeout(FETCH_TIMEOUT, exchange).await.unwrap_or_else(|_| Err(anyhow::Error::new(Lost("APPEND timed out".into()))));
    session.finished(r)
}

/// Deletes a draft's copies from `mailbox`: the one on record, `known` (`(UIDVALIDITY, UID)`),
/// and, with `search`, any others the Message-ID search finds (headers checked). Returns their
/// UIDs.
pub async fn purge_message_id(
    session: &mut Connection<impl Io>,
    caps: Caps,
    mailbox: &str,
    message_id: &str,
    known: Option<(u32, u32)>,
    search: bool,
) -> Result<Vec<u32>> {
    let validity = wire::select(session, mailbox).await?.uid_validity;
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
async fn uids_with_message_id(session: &mut Connection<impl Io>, message_id: &str) -> Result<Vec<u32>> {
    let query = format!("UNDELETED HEADER Message-ID {}", quoted(message_id));
    let hits = wire::uid_search(session, &query).await?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let set = hits.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let fetched = wire::fetch(session, true, &set, "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])", "FETCH ids").await?;
    Ok(fetched
        .iter()
        .filter(|f| f.header.as_deref().and_then(|h| mime::parse(h, true)).is_some_and(|p| p.message_id == message_id))
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

pub async fn capabilities(session: &mut Connection<impl Io>) -> Result<Caps> {
    let caps = wire::capabilities(session).await?;
    Ok(Caps { moves: caps.contains("MOVE"), idle: caps.contains("IDLE"), uidplus: caps.contains("UIDPLUS") })
}

/// Waits on IDLE for up to `wait`, returning early when the server reports a change or
/// `interrupt` fires. Gives the session back either way.
pub async fn idle<T: Io>(session: Connection<T>, wait: Duration, interrupt: Arc<tokio::sync::Notify>) -> Result<(Connection<T>, bool)> {
    session.usable()?;
    let unsolicited = session.inner.unsolicited_responses.clone();
    let mut handle = session.inner.idle();
    timed("IDLE", handle.init()).await?.map_err(lost_if_transport)?;
    // Mail that arrived since the caller last looked ([`changed_since`]) may be told before the
    // server's go-ahead: async-imap files that away rather than ending the wait, so it's looked
    // for here, and the wait skipped.
    let changed = news_in(|| unsolicited.try_recv().ok()) || {
        // async-imap's own limit starts again at every response, keepalives (`* OK Still
        // here`) included, so a server sending them more often than `wait` would hold the wait
        // open for ever: `wait` is kept here, from the start.
        let (fut, stop) = handle.wait_with_timeout(wait);
        tokio::pin!(fut);
        let ended = tokio::select! {
            r = &mut fut => Some(r),
            _ = interrupt.notified() => None,
            _ = tokio::time::sleep(wait) => None,
        };
        let r = match ended {
            Some(r) => r,
            // Ask the wait to stop, and let it finish reading cleanly.
            None => {
                drop(stop);
                timed("IDLE stop", &mut fut).await?
            }
        };
        matches!(r.map_err(lost_if_transport)?, async_imap::extensions::idle::IdleResponse::NewData(_))
    };
    // Told as the IDLE ended, before its OK: filed away too.
    let mut session = Connection::new(timed("DONE", handle.done()).await?.map_err(lost_if_transport)?);
    let changed = news(&mut session) | changed;
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
    /// `TAG OK done`. An answer ending in [`HANG_UP`] closes the connection after what comes
    /// before it, as a server does with one left idle too long (or going away mid-answer);
    /// [`SILENT`] answers nothing. One ending in [`every`]'s line goes on sending that line
    /// until the client's next command (an IDLE's keepalives).
    pub async fn session(answer: impl Fn(&str) -> Option<String> + Send + 'static) -> (super::Connection<DuplexStream>, Log) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let log: Log = Arc::default();
        let seen = log.clone();
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(server);
            let mut r = BufReader::new(r);
            let w = Arc::new(tokio::sync::Mutex::new(w));
            w.lock().await.write_all(b"* OK fake ready\r\n").await.unwrap();
            let mut line = String::new();
            let mut last_tag = String::new();
            let mut repeating: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                line.clear();
                let read = r.read_line(&mut line).await.unwrap_or(0);
                if let Some(task) = repeating.take() {
                    task.abort();
                }
                if read == 0 {
                    return;
                }
                // IDLE's DONE has no tag: it ends the command before it.
                let (tag, command) = match line.trim_end() {
                    "DONE" => (last_tag.clone(), "DONE".to_owned()),
                    l => match l.split_once(' ') {
                        Some((tag, command)) => (tag.to_owned(), command.to_owned()),
                        None => return,
                    },
                };
                last_tag = tag.clone();
                // A literal follows: ask for it and read it (plus the line's end).
                if let Some(n) = command.strip_suffix('}').and_then(|c| c.rsplit_once('{')).and_then(|(_, n)| n.parse::<usize>().ok()) {
                    w.lock().await.write_all(b"+ go ahead\r\n").await.unwrap();
                    let mut literal = vec![0; n + 2];
                    r.read_exact(&mut literal).await.unwrap();
                }
                seen.lock().unwrap().push(command.clone());
                let reply = if command.starts_with("LOGIN") { None } else { answer(&command) };
                let reply = reply.unwrap_or_else(|| "TAG OK done\r\n".into()).replace("TAG", &tag);
                // No answer, the connection left open: a dead one's packets go nowhere.
                if reply == SILENT {
                    continue;
                }
                // Some of an answer, then the connection closes.
                if let Some(before) = reply.strip_suffix(HANG_UP) {
                    let mut w = w.lock().await;
                    let _ = w.write_all(before.as_bytes()).await;
                    let _ = w.shutdown().await;
                    return;
                }
                let (reply, again) = match reply.split_once(EVERY) {
                    Some((reply, again)) => (reply.to_owned(), again.split_once(' ').map(|(ms, l)| (ms.parse().unwrap(), l.to_owned()))),
                    None => (reply, None),
                };
                w.lock().await.write_all(reply.as_bytes()).await.unwrap();
                if let Some((ms, again)) = again {
                    let w = w.clone();
                    repeating = Some(tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                            if w.lock().await.write_all(again.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }));
                }
            }
        });
        let mut c = async_imap::Client::new(client);
        c.read_response().await.unwrap().unwrap();
        (super::Connection::new(c.login("me", "pw").await.map_err(|e| e.0).unwrap()), log)
    }

    /// What an answer ends with to close the connection there.
    pub const HANG_UP: &str = "<hang up>";

    /// An answer that never comes, the connection staying open: one dropped without a word.
    pub const SILENT: &str = "<silent>";

    const EVERY: &str = "<every>";

    /// What an answer ends with to go on sending `line` every `ms` milliseconds after it, until
    /// the client's next command.
    pub fn every(ms: u64, line: &str) -> String {
        format!("{EVERY}{ms} {line}")
    }

    /// A SELECT's answer.
    pub fn selected(validity: u32, uid_next: u32) -> String {
        format!("* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* 3 EXISTS\r\n* OK [UIDVALIDITY {validity}] ok\r\n* OK [UIDNEXT {uid_next}] ok\r\nTAG OK [READ-WRITE] done\r\n")
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

    /// New mail is fetched with the headers the split inbox reads, so it's sorted as it lands;
    /// and a header block of that shape is read into its signals.
    #[test]
    fn new_mail_comes_with_the_headers_the_split_inbox_reads() {
        let fields = HEADER_QUERY.split_once("HEADER.FIELDS (").unwrap().1.trim_end_matches(")])");
        for h in mime::BULK_HEADERS.split_whitespace() {
            assert!(fields.split_whitespace().any(|f| f == h), "{h} isn't fetched: {fields}");
        }
        let block = b"From: GitHub <notifications@github.com>\r\nTo: me@x.example\r\nSubject: [x] PR\r\nList-ID: x/y <y.x.github.com>\r\nX-GitHub-Reason: mention\r\n\r\n";
        let p = mime::parse(block, true).unwrap();
        assert_eq!(p.bulk_headers.list_id, "y.x.github.com");
        assert!(p.bulk_headers.platform && !p.bulk_headers.list_unsubscribe);
    }

    /// A mailbox of `n` messages (UID = sequence number + 100), the ones in `deleted` flagged
    /// \Deleted, answering SELECT and header FETCHes by sequence range.
    fn mailbox_server(n: u32, deleted: std::ops::RangeInclusive<u32>) -> impl Fn(&str) -> Option<String> + Send + 'static {
        move |c: &str| {
            if c.starts_with("SELECT") {
                return Some(format!("* FLAGS (\\Seen \\Deleted)\r\n* {n} EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT {}] ok\r\nTAG OK [READ-WRITE] done\r\n", n + 101));
            }
            let range = c.strip_prefix("FETCH ")?.split(' ').next()?;
            let (lo, hi) = range.split_once(':')?;
            let (lo, hi): (u32, u32) = (lo.parse().ok()?, hi.parse().ok()?);
            let mut out = String::new();
            for seq in lo..=hi.min(n) {
                let uid = seq + 100;
                let flags = if deleted.contains(&seq) { "\\Deleted" } else { "" };
                let header = format!("From: a@x.com\r\nSubject: m{uid}\r\nMessage-ID: <{uid}@x>\r\n\r\n");
                out.push_str(&format!(
                    "* {seq} FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE \"03-Mar-2026 10:00:00 +0000\" RFC822.SIZE 100 BODY[HEADER.FIELDS (FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)] {{{}}}\r\n{header})\r\n",
                    header.len()
                ));
            }
            Some(out + "TAG OK done\r\n")
        }
    }

    #[tokio::test]
    async fn a_connection_the_server_closed_is_lost_not_an_empty_mailbox() {
        let store = Mutex::new(Store::memory());
        store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        let (mut s, _log) = fake::session(mailbox_server(3, 0..=0)).await;
        sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.unwrap();
        let cached = |store: &Mutex<Store>| store.lock().unwrap().local_uids("a", "INBOX").unwrap().len();
        assert_eq!(cached(&store), 3);
        // The server hangs up (the connection sat idle too long). async-imap reads that as a
        // SELECT answered with nothing: that mustn't empty the local copy, or pass at all.
        let (mut s, _log) = fake::session(|_| Some(fake::HANG_UP.to_owned())).await;
        let Err(e) = sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await else { panic!("a closed connection synced") };
        assert!(is_lost(&e), "{e:#}");
        assert_eq!(cached(&store), 3, "the local copy is kept");
        let (mut s, _log) = fake::session(|_| Some(fake::HANG_UP.to_owned())).await;
        assert!(select(&mut s, "INBOX").await.is_err());
        let (mut s, _log) = fake::session(|_| Some(fake::HANG_UP.to_owned())).await;
        assert!(changed_since(&mut s, "INBOX", None).await.is_err());
        let (mut s, _log) = fake::session(|_| Some(fake::HANG_UP.to_owned())).await;
        assert!(status(&mut s, "INBOX").await.is_err(), "not zero unread of zero");
    }

    // ── Answers are trusted only once the server finishes them (its tagged OK) ──

    /// How an answer goes wrong partway: the connection closes, the server refuses (tagged
    /// NO or BAD), or says BYE and goes.
    #[derive(Clone, Copy, Debug)]
    enum Breaks {
        Eof,
        No,
        Bad,
        Bye,
    }

    const BREAKS: [Breaks; 4] = [Breaks::Eof, Breaks::No, Breaks::Bad, Breaks::Bye];

    /// `partial` (some of a command's answer), then it goes wrong `how`.
    fn broken(partial: &str, how: Breaks) -> String {
        match how {
            Breaks::Eof => format!("{partial}{}", fake::HANG_UP),
            Breaks::No => format!("{partial}TAG NO [UNAVAILABLE] try later\r\n"),
            Breaks::Bad => format!("{partial}TAG BAD what was that\r\n"),
            Breaks::Bye => format!("{partial}* BYE going away\r\n{}", fake::HANG_UP),
        }
    }

    const SELECT_INBOX: &str = "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* 3 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT 104] ok\r\nTAG OK [READ-WRITE] done\r\n";

    /// A server whose INBOX holds UIDs 101–103 (sequence 1–3, UIDVALIDITY 7; 101 read) and a
    /// draft in Drafts, answering everything Envoy asks in full.
    fn inbox_server(c: &str) -> Option<String> {
        let header = |uid: u32| format!("From: a@x.com\r\nSubject: m{uid}\r\nMessage-ID: <{uid}@x>\r\n\r\n");
        let headers = |seqs: std::ops::RangeInclusive<u32>| {
            seqs.map(|seq| {
                let (uid, h) = (seq + 100, header(seq + 100));
                let flags = if uid == 101 { "\\Seen" } else { "" };
                format!("* {seq} FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE \"03-Mar-2026 10:00:00 +0000\" RFC822.SIZE 100 BODY[HEADER.FIELDS (FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)] {{{}}}\r\n{h})\r\n", h.len())
            })
            .collect::<String>()
        };
        Some(match c {
            c if c.starts_with("SELECT") => SELECT_INBOX.to_owned(),
            c if c.starts_with("FETCH 1:3 ") => headers(1..=3) + "TAG OK done\r\n",
            c if c.starts_with("UID FETCH 104:* ") => "TAG OK done\r\n".to_owned(),
            "UID FETCH 101:103 (UID FLAGS)" => "* 1 FETCH (UID 101 FLAGS (\\Seen))\r\n* 2 FETCH (UID 102 FLAGS ())\r\n* 3 FETCH (UID 103 FLAGS ())\r\nTAG OK done\r\n".to_owned(),
            c if c.starts_with("UID FETCH 101 (UID BODY.PEEK[])") => {
                let body = format!("{}hi\r\n", header(101));
                format!("* 1 FETCH (UID 101 BODY[] {{{}}}\r\n{body})\r\nTAG OK done\r\n", body.len())
            }
            c if c.starts_with("STATUS") => "* STATUS INBOX (MESSAGES 3 UNSEEN 2)\r\nTAG OK done\r\n".to_owned(),
            c if c.starts_with("LIST") => "* LIST (\\HasNoChildren) \"/\" INBOX\r\n* LIST (\\HasNoChildren \\Drafts) \"/\" Drafts\r\n* LIST (\\HasNoChildren) \"/\" Work\r\nTAG OK done\r\n".to_owned(),
            c if c.starts_with("UID SEARCH") => "* SEARCH 5\r\nTAG OK done\r\n".to_owned(),
            c if c.starts_with("UID FETCH 5 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])") => fake::message_ids(&[(5, "<d@envoy>")]),
            _ => return None,
        })
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// [`inbox_server`], except that its answer to the command starting with `target` is
    /// `partial` and then goes wrong `how`.
    fn breaking(target: &'static str, partial: &'static str, how: Breaks) -> impl Fn(&str) -> Option<String> + Send + 'static {
        move |c: &str| if c.starts_with(target) { Some(broken(partial, how)) } else { inbox_server(c) }
    }

    /// A store with INBOX synced from [`inbox_server`]: three messages, UIDVALIDITY 7, counts
    /// 2 unread of 3.
    async fn synced_store() -> Mutex<Store> {
        let store = Mutex::new(Store::memory());
        store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        let (mut s, _log) = fake::session(inbox_server).await;
        sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.unwrap();
        store.lock().unwrap().set_counts("a", "INBOX", 2, 3).unwrap();
        assert_eq!(cache(&store), (vec![101, 102, 103], 7, (2, 3)));
        store
    }

    /// INBOX as cached: its UIDs, UIDVALIDITY, and (unread, total).
    fn cache(store: &Mutex<Store>) -> (Vec<u32>, u32, (u32, u32)) {
        let st = store.lock().unwrap();
        let inbox = st.mailboxes("a").unwrap().into_iter().find(|m| m.name == "INBOX").unwrap();
        (st.local_uids("a", "INBOX").unwrap().into_iter().map(|(u, _)| u).collect(), st.uidvalidity("a", "INBOX").unwrap(), (inbox.unread, inbox.total))
    }

    #[tokio::test]
    async fn a_sync_cut_short_anywhere_changes_nothing_here() {
        // Each command a sync of a cached mailbox sends, cut short each way, with some of its
        // answer in first: the SELECT says FLAGS but not yet UIDVALIDITY; the flag listing
        // has 101 but not 102 and 103.
        let cases: [(&str, &str); 3] = [
            ("SELECT", "* FLAGS (\\Seen \\Deleted)\r\n"),
            ("UID FETCH 104:* ", ""),
            ("UID FETCH 101:103 (UID FLAGS)", "* 1 FETCH (UID 101 FLAGS (\\Seen))\r\n"),
        ];
        for (target, partial) in cases {
            for how in BREAKS {
                let store = synced_store().await;
                let (mut s, _log) = fake::session(breaking(target, partial, how)).await;
                let r = sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await;
                assert!(r.is_err(), "{target} {how:?}: a sync cut short passed");
                assert_eq!(cache(&store), (vec![101, 102, 103], 7, (2, 3)), "{target} {how:?}: the cache changed");
            }
        }
    }

    #[tokio::test]
    async fn a_first_download_cut_short_isnt_success() {
        for how in BREAKS {
            let store = Mutex::new(Store::memory());
            store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
            let h = "Subject: m101\r\n\r\n";
            let one = leak(format!("* 1 FETCH (UID 101 FLAGS () BODY[HEADER.FIELDS (FROM SUBJECT)] {{{}}}\r\n{h})\r\n", h.len()));
            let (mut s, _log) = fake::session(breaking("FETCH 1:3 ", one, how)).await;
            assert!(sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.is_err(), "{how:?}");
        }
    }

    #[tokio::test]
    async fn a_select_must_say_flags_exists_and_uidvalidity() {
        // Finished with a tagged OK, but missing what every SELECT says: not an empty mailbox.
        let incomplete = [
            "* FLAGS (\\Seen)\r\n* OK [UIDVALIDITY 7] ok\r\nTAG OK done\r\n",
            "* FLAGS (\\Seen)\r\n* 3 EXISTS\r\nTAG OK done\r\n",
            "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\nTAG OK done\r\n",
        ];
        for answer in incomplete {
            let store = synced_store().await;
            let (mut s, _log) = fake::session(move |c| if c.starts_with("SELECT") { Some(answer.to_owned()) } else { inbox_server(c) }).await;
            assert!(sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.is_err(), "{answer:?}");
            assert_eq!(cache(&store), (vec![101, 102, 103], 7, (2, 3)), "{answer:?}");
        }
        // A mailbox that really is empty now still syncs empty.
        let store = synced_store().await;
        let (mut s, _log) = fake::session(|c| {
            if c.starts_with("SELECT") { Some("* FLAGS (\\Seen)\r\n* 0 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\nTAG OK done\r\n".into()) } else { inbox_server(c) }
        })
        .await;
        sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.unwrap();
        assert!(cache(&store).0.is_empty(), "emptied on the server: emptied here");
    }

    #[tokio::test]
    async fn status_needs_every_count_it_asked_for() {
        for how in BREAKS {
            let (mut s, _log) = fake::session(breaking("STATUS", "* STATUS INBOX (MESSAGES 3 UNSEEN 2)\r\n", how)).await;
            assert!(status(&mut s, "INBOX").await.is_err(), "{how:?}");
        }
        for answer in ["* STATUS INBOX (MESSAGES 3)\r\nTAG OK done\r\n", "* STATUS INBOX (UNSEEN 2)\r\nTAG OK done\r\n", "TAG OK done\r\n"] {
            let (mut s, _log) = fake::session(move |c| c.starts_with("STATUS").then(|| answer.to_owned())).await;
            assert!(status(&mut s, "INBOX").await.is_err(), "{answer:?}: not zero");
        }
        let (mut s, _log) = fake::session(inbox_server).await;
        assert_eq!(status(&mut s, "INBOX").await.unwrap(), (2, 3));
    }

    #[test]
    fn contents_move_on_with_mail_in_or_out() {
        let was = Contents { exists: 3, uid_next: Some(10), uid_validity: Some(7) };
        assert!(!was.moved_on(was));
        // One moved in and another out: the same count, a new UID.
        assert!(was.moved_on(Contents { uid_next: Some(11), ..was }));
        assert!(was.moved_on(Contents { exists: 2, ..was }));
        // Deleted and made again with as many messages, and as many UIDs used: a new generation.
        assert!(was.moved_on(Contents { uid_validity: Some(8), ..was }));
        // Without UIDNEXT or UIDVALIDITY on one side, by the rest.
        assert!(!was.moved_on(Contents { uid_next: None, uid_validity: None, ..was }));
        assert!(Contents { uid_next: None, ..was }.moved_on(Contents { exists: 4, uid_next: Some(11), ..was }));
    }

    #[tokio::test]
    async fn a_status_tells_contents_with_or_without_uidnext() {
        let (mut s, log) = fake::session(|c| match c {
            "STATUS \"Old Projects\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => Some("* STATUS \"Old Projects\" (MESSAGES 4 UNSEEN 1 UIDNEXT 12 UIDVALIDITY 7)\r\nTAG OK done\r\n".into()),
            "STATUS \"Junk\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => Some("* STATUS \"Junk\" (MESSAGES 4 UNSEEN 0)\r\nTAG OK done\r\n".into()),
            _ => Some("* STATUS \"Other\" (MESSAGES 9 UNSEEN 0 UIDNEXT 99)\r\n* STATUS \"Trash\" (UNSEEN 0 UIDNEXT 12)\r\nTAG OK done\r\n".into()),
        })
        .await;
        assert_eq!(contents(&mut s, "Old Projects").await.unwrap(), Contents { exists: 4, uid_next: Some(12), uid_validity: Some(7) });
        assert_eq!(contents(&mut s, "Junk").await.unwrap(), Contents { exists: 4, uid_next: None, uid_validity: None });
        // Another mailbox's count isn't this one's.
        assert!(contents(&mut s, "Trash").await.is_err());
        assert_eq!(log.lock().unwrap().last().unwrap(), "STATUS \"Trash\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)");
    }

    #[tokio::test]
    async fn a_mailbox_list_cut_short_isnt_the_list() {
        for how in BREAKS {
            let (mut s, _log) = fake::session(breaking("LIST", "* LIST (\\HasNoChildren) \"/\" INBOX\r\n", how)).await;
            assert!(list_mailboxes(&mut s).await.is_err(), "{how:?}: Drafts and Work would be forgotten");
        }
        let (mut s, _log) = fake::session(inbox_server).await;
        assert_eq!(list_mailboxes(&mut s).await.unwrap().iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["INBOX", "Drafts", "Work"]);
    }

    #[tokio::test]
    async fn bodies_cut_short_store_nothing() {
        for how in BREAKS {
            let store = synced_store().await;
            let partial = "* 1 FETCH (UID 101 BODY[] {12}\r\nSubject: x\r\n)\r\n";
            let (mut s, _log) = fake::session(breaking("UID FETCH 101 (UID BODY.PEEK[])", partial, how)).await;
            assert!(fetch_bodies(&mut s, &store, "a", "INBOX", 7, &[101]).await.is_err(), "{how:?}");
        }
        let store = synced_store().await;
        let (mut s, _log) = fake::session(inbox_server).await;
        assert_eq!(fetch_bodies(&mut s, &store, "a", "INBOX", 7, &[101]).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bodies_are_stored_only_in_the_generation_they_were_asked_for_in() {
        // Cached in generation 7; the server reset INBOX since, and UID 101 is another message
        // in generation 8.
        let store = synced_store().await;
        let row = store.lock().unwrap().id_for_uid("a", "INBOX", 101).unwrap().unwrap();
        let reset = |c: &str| match c {
            c if c.starts_with("SELECT") => Some(SELECT_INBOX.replace("UIDVALIDITY 7", "UIDVALIDITY 8")),
            c if c.starts_with("UID FETCH 101 (UID BODY.PEEK[])") => {
                let body = "From: b@x.com\r\nSubject: someone else's\r\nMessage-ID: <other@x>\r\n\r\nnot yours\r\n";
                Some(format!("* 1 FETCH (UID 101 BODY[] {{{}}}\r\n{body})\r\nTAG OK done\r\n", body.len()))
            }
            c => inbox_server(c),
        };
        let (mut s, log) = fake::session(reset).await;
        let e = fetch_bodies(&mut s, &store, "a", "INBOX", 7, &[101]).await.unwrap_err();
        assert_eq!(e.downcast_ref::<NewGeneration>(), Some(&NewGeneration { mailbox: "INBOX".into(), validity: 8, found: None }), "{e:#}");
        assert!(!store.lock().unwrap().has_body(row), "the other message's body isn't stored as this one's");
        assert!(!commands(&log).iter().any(|c| c.starts_with("UID FETCH 101 (UID BODY")), "{:?}", commands(&log));
        // Asked for in 7, and the server still in 7, but the cache moved on meanwhile: nothing.
        store.lock().unwrap().set_uidvalidity("a", "INBOX", 9).unwrap();
        let (mut s, _log) = fake::session(inbox_server).await;
        assert!(fetch_bodies(&mut s, &store, "a", "INBOX", 7, &[101]).await.unwrap_err().downcast_ref::<NewGeneration>().is_some());
        assert!(!store.lock().unwrap().has_body(row));
    }

    #[tokio::test]
    async fn a_search_cut_short_deletes_nothing() {
        let found = leak(fake::message_ids(&[(5, "<d@envoy>")]).replace("TAG OK done\r\n", ""));
        for (target, partial) in [("UID SEARCH", "* SEARCH 5\r\n"), ("UID FETCH 5 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])", found)] {
            for how in BREAKS {
                let (mut s, log) = fake::session(breaking(target, partial, how)).await;
                assert!(purge_message_id(&mut s, caps(true), "Drafts", "<d@envoy>", None, true).await.is_err(), "{target} {how:?}");
                assert!(!commands(&log).iter().any(|c| c.starts_with("UID STORE") || c.contains("EXPUNGE")), "{target} {how:?}: {:?}", commands(&log));
            }
        }
    }

    #[tokio::test]
    async fn changes_the_server_didnt_finish_arent_done() {
        // Each command a queued change replays, cut short each way: the op must stay queued
        // (an error), never be taken as done (its message deleted here, a deletion reported).
        for how in BREAKS {
            let (mut s, _log) = fake::session(breaking("UID STORE", "* 1 FETCH (UID 101 FLAGS (\\Seen))\r\n", how)).await;
            assert!(store_flags(&mut s, "INBOX", &at(101), flags::SEEN, 0).await.is_err(), "STORE {how:?}");
            // Without UIDPLUS the \Deleted STORE is the whole deletion.
            let (mut s, _log) = fake::session(breaking("UID STORE", "", how)).await;
            assert!(expunge(&mut s, caps(false), "INBOX", &at(101)).await.is_err(), "STORE \\Deleted {how:?}");
            let (mut s, _log) = fake::session(breaking("UID EXPUNGE", "* 1 EXPUNGE\r\n", how)).await;
            assert!(expunge(&mut s, caps(true), "INBOX", &at(101)).await.is_err(), "UID EXPUNGE {how:?}");
            let (mut s, _log) = fake::session(breaking("UID COPY", "", how)).await;
            assert!(copy_message(&mut s, "INBOX", &at(101), "Work").await.is_err(), "COPY {how:?}");
            let (mut s, log) = fake::session(breaking("UID COPY", "", how)).await;
            assert!(move_message(&mut s, caps(false), "INBOX", &at(101), "Work").await.is_err(), "COPY to move {how:?}");
            assert!(!commands(&log).iter().any(|c| c.starts_with("UID STORE")), "not deleted where it was: {how:?}");
            let (mut s, _log) = fake::session(breaking("UID MOVE", "* 1 EXPUNGE\r\n", how)).await;
            assert!(move_message(&mut s, Caps { moves: true, ..caps(true) }, "INBOX", &at(101), "Work").await.is_err(), "MOVE {how:?}");
            let (mut s, _log) = fake::session(breaking("APPEND", "", how)).await;
            assert!(append(&mut s, "Sent", b"Subject: x\r\n\r\nhi\r\n", true).await.is_err(), "APPEND {how:?}");
        }
    }

    #[tokio::test]
    async fn a_connection_closing_inside_a_literal_is_lost() {
        // The body's literal promises 100 bytes; the connection closes after a few. async-imap
        // reports that as an I/O error ("bytes remaining in stream"), not a closed connection.
        let store = synced_store().await;
        let (mut s, _log) = fake::session(breaking("UID FETCH 101 (UID BODY.PEEK[])", "* 1 FETCH (UID 101 BODY[] {100}\r\nSubject: x\r\n", Breaks::Eof)).await;
        let e = fetch_bodies(&mut s, &store, "a", "INBOX", 7, &[101]).await.unwrap_err();
        assert!(e.downcast_ref::<Lost>().is_some(), "{e:#}: not taken for a lost connection");
    }

    #[tokio::test]
    async fn a_session_whose_answer_didnt_finish_isnt_used_again() {
        // A command times out with its answer half told: what's left of it could be read as
        // the next command's. The session is poisoned: nothing more is sent on it.
        let (mut s, log) = fake::session(|c: &str| match c {
            "NOOP" => Some("* 7 EXISTS\r\n".into()),
            c => inbox_server(c),
        })
        .await;
        let e = wire::run(&mut s, "NOOP", "NOOP", Duration::from_millis(200), &mut |_| {}).await.unwrap_err();
        assert!(e.downcast_ref::<Lost>().is_some(), "{e:#}");
        let e = select(&mut s, "INBOX").await.unwrap_err();
        assert!(e.downcast_ref::<Lost>().is_some(), "{e:#}");
        assert!(!commands(&log).iter().any(|c| c.starts_with("SELECT")), "{:?}", commands(&log));
    }

    #[tokio::test]
    async fn messages_flagged_deleted_dont_fill_the_first_download() {
        // The newest four of ten are flagged \Deleted (removed elsewhere, not expunged yet).
        let (mut s, _log) = fake::session(mailbox_server(10, 7..=10)).await;
        let store = Mutex::new(Store::memory());
        store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        sync_mailbox(&mut s, &store, "a", "INBOX", 5, &|| {}).await.unwrap();
        let st = store.lock().unwrap();
        let mut shown: Vec<u32> = st
            .list(&crate::model::View::Mailbox { account: "a".into(), mailbox: "INBOX".into() }, &crate::store::Query::default(), false, 100, &HashSet::new())
            .unwrap()
            .iter()
            .map(|m| m.uid)
            .collect();
        shown.sort_unstable();
        assert_eq!(shown, vec![102, 103, 104, 105, 106], "five messages to show, not one");
    }

    #[tokio::test]
    async fn flag_updates_in_among_a_first_download_dont_count_as_messages() {
        // Message 6 is read on another computer meanwhile: each answer also tells of that.
        let inner = mailbox_server(10, 7..=10);
        let (mut s, _log) = fake::session(move |c| {
            let answer = inner(c)?;
            Some(if c.starts_with("FETCH ") { format!("* 6 FETCH (FLAGS (\\Seen) UID 106)\r\n{answer}") } else { answer })
        })
        .await;
        let store = Mutex::new(Store::memory());
        store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        sync_mailbox(&mut s, &store, "a", "INBOX", 5, &|| {}).await.unwrap();
        let st = store.lock().unwrap();
        let view = crate::model::View::Mailbox { account: "a".into(), mailbox: "INBOX".into() };
        let mut shown: Vec<u32> = st.list(&view, &crate::store::Query::default(), false, 100, &HashSet::new()).unwrap().iter().map(|m| m.uid).collect();
        shown.sort_unstable();
        assert_eq!(shown, vec![102, 103, 104, 105, 106], "five messages to show");
    }

    #[tokio::test]
    async fn mail_delivered_after_a_sync_is_noticed_before_idle() {
        let delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Nothing flagged \Deleted (there's no message 0).
        let (d, inner) = (delivered.clone(), mailbox_server(10, 0..=0));
        let (mut s, _log) = fake::session(move |c| {
            if c.starts_with("SELECT") && d.load(std::sync::atomic::Ordering::SeqCst) {
                return Some("* FLAGS (\\Seen \\Deleted)\r\n* 11 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT 112] ok\r\nTAG OK [READ-WRITE] done\r\n".into());
            }
            inner(c)
        })
        .await;
        let store = Mutex::new(Store::memory());
        store.lock().unwrap().upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        let synced = sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.unwrap().contents;
        assert!(!changed_since(&mut s, "INBOX", Some(synced)).await.unwrap(), "nothing came since");
        delivered.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(changed_since(&mut s, "INBOX", Some(synced)).await.unwrap(), "a message delivered after the sync");
        assert!(changed_since(&mut s, "INBOX", None).await.unwrap(), "never synced");
    }

    /// A server whose inbox holds 3 messages (UIDNEXT 11) when selected, and that answers IDLE
    /// with `idle` (which ends with the go-ahead). An earlier NOOP was told of 150 changes
    /// unasked, more than async-imap's channel for them holds.
    fn idle_server(idle: &'static str) -> impl Fn(&str) -> Option<String> + Send + 'static {
        move |c: &str| match c {
            "NOOP" => Some((1..=150).map(|n| format!("* {n} EXISTS\r\n")).collect::<String>() + "TAG OK done\r\n"),
            "IDLE" => Some(idle.into()),
            c if c.starts_with("SELECT") => Some(fake::selected(7, 11)),
            _ => None,
        }
    }

    const SYNCED: Contents = Contents { exists: 3, uid_next: Some(11), uid_validity: Some(7) };

    #[tokio::test]
    async fn mail_told_before_idles_go_ahead_is_noticed() {
        // Delivered between the check and IDLE: the server tells of it before its "+".
        let (mut s, log) = fake::session(idle_server("* 4 EXISTS\r\n* 1 RECENT\r\n+ idling\r\n")).await;
        s.inner.noop().await.unwrap();
        assert!(!changed_since(&mut s, "INBOX", Some(SYNCED)).await.unwrap(), "what was told before the check is older than it");
        let notify = Arc::new(tokio::sync::Notify::new());
        let (_s, news) = tokio::time::timeout(Duration::from_secs(5), idle(s, Duration::from_secs(60), notify)).await.expect("no waiting for the timeout").unwrap();
        assert!(news, "a sync follows at once");
        assert_eq!(commands(&log)[commands(&log).len() - 2..], ["IDLE", "DONE"]);
    }

    #[tokio::test]
    async fn idle_without_news_waits_it_out() {
        let (mut s, _log) = fake::session(idle_server("+ idling\r\n")).await;
        s.inner.noop().await.unwrap();
        assert!(!changed_since(&mut s, "INBOX", Some(SYNCED)).await.unwrap());
        let started = std::time::Instant::now();
        let (_s, news) = idle(s, Duration::from_millis(300), Arc::new(tokio::sync::Notify::new())).await.unwrap();
        assert!(!news && started.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn keepalives_dont_stretch_idle_past_its_wait() {
        // A server that says it's still there every 50 ms while idling (as many do, every 10 to
        // 20 s): the wait still ends on time, with no news, and the IDLE is ended cleanly.
        let keepalives = format!("+ idling\r\n{}", fake::every(50, "* OK Still here\r\n"));
        let (mut s, log) = fake::session(move |c| if c == "IDLE" { Some(keepalives.clone()) } else { idle_server("")(c) }).await;
        assert!(!changed_since(&mut s, "INBOX", Some(SYNCED)).await.unwrap());
        let started = std::time::Instant::now();
        let waited = tokio::time::timeout(Duration::from_secs(3), idle(s, Duration::from_millis(300), Arc::new(tokio::sync::Notify::new()))).await;
        let (mut s, news) = waited.expect("the wait ends at its deadline, keepalives or not").unwrap();
        assert!(!news && started.elapsed() >= Duration::from_millis(300));
        assert_eq!(commands(&log)[commands(&log).len() - 2..], ["IDLE", "DONE"]);
        // Done with cleanly: the connection goes on.
        select(&mut s, "INBOX").await.unwrap();
    }

    /// A message by UID in generation 7, the fake servers' UIDVALIDITY.
    fn at(uid: u32) -> Target<'static> {
        Target { uid, validity: 7, message_id: "" }
    }

    /// A server whose INBOX is in generation 8 (it was reset since generation 7), where
    /// `<moved@x>` is UID 5 now, `<twice@x>` is two messages, and `<gone@x>` none.
    fn reset_server(c: &str) -> Option<String> {
        Some(match c {
            c if c.starts_with("SELECT") => fake::selected(8, 9),
            "UID SEARCH UNDELETED HEADER Message-ID \"<moved@x>\"" => "* SEARCH 5\r\nTAG OK done\r\n".into(),
            "UID SEARCH UNDELETED HEADER Message-ID \"<twice@x>\"" => "* SEARCH 5 6\r\nTAG OK done\r\n".into(),
            c if c.starts_with("UID SEARCH") => "* SEARCH\r\nTAG OK done\r\n".into(),
            c if c.starts_with("UID FETCH 5 ") => fake::message_ids(&[(5, "<moved@x>")]),
            c if c.starts_with("UID FETCH 5,6 ") || c.starts_with("UID FETCH 6,5 ") => fake::message_ids(&[(5, "<twice@x>"), (6, "<twice@x>")]),
            _ => return None,
        })
    }

    #[tokio::test]
    async fn an_op_never_runs_on_a_uid_from_another_generation() {
        // Queued against generation 7 for UID 1. The mailbox is generation 8 now: UID 1 may be
        // anything. Nothing is sent with it; the message is looked for by its Message-ID.
        let target = |message_id| Target { uid: 1, validity: 7, message_id };
        let cases = [("<moved@x>", Some(5)), ("<twice@x>", None), ("<gone@x>", None), ("", None)];
        for (message_id, found) in cases {
            let want = NewGeneration { mailbox: "INBOX".into(), validity: 8, found };
            let new_generation = |r: Result<()>| r.unwrap_err().downcast::<NewGeneration>().unwrap();
            let (mut s, log) = fake::session(reset_server).await;
            assert_eq!(new_generation(move_message(&mut s, Caps { moves: true, ..caps(true) }, "INBOX", &target(message_id), "Archive").await), want, "{message_id}");
            assert_eq!(new_generation(move_message(&mut s, caps(false), "INBOX", &target(message_id), "Archive").await), want);
            assert_eq!(new_generation(copy_message(&mut s, "INBOX", &target(message_id), "Work").await), want);
            assert_eq!(new_generation(store_flags(&mut s, "INBOX", &target(message_id), flags::SEEN, 0).await), want);
            assert_eq!(new_generation(expunge(&mut s, caps(true), "INBOX", &target(message_id)).await), want);
            let sent: Vec<String> = commands(&log).into_iter().filter(|c| c.starts_with("UID MOVE") || c.starts_with("UID COPY") || c.starts_with("UID STORE") || c.starts_with("UID EXPUNGE")).collect();
            assert!(sent.is_empty(), "{message_id}: {sent:?}");
        }
        // Not knowing the generation (an op from before it was recorded, its mailbox gone) is
        // never taken for knowing it.
        let (mut s, log) = fake::session(reset_server).await;
        assert!(move_message(&mut s, caps(true), "INBOX", &Target { uid: 1, validity: 0, message_id: "" }, "Archive").await.is_err());
        assert!(!commands(&log).iter().any(|c| c.starts_with("UID COPY")));
        // The same generation: it runs.
        let (mut s, log) = fake::session(reset_server).await;
        move_message(&mut s, Caps { moves: true, ..caps(true) }, "INBOX", &Target { uid: 1, validity: 8, message_id: "" }, "Archive").await.unwrap();
        assert_eq!(commands(&log).last().map(String::as_str), Some("UID MOVE 1 \"Archive\""));
    }

    #[tokio::test]
    async fn a_reset_keeps_messages_waiting_to_move_hidden() {
        let store = synced_store().await;
        // 102 is archived, undoably: hidden while its move waits.
        let op = {
            let st = store.lock().unwrap();
            let id = st.id_for_uid("a", "INBOX", 102).unwrap().unwrap();
            let op = st.enqueue_held("a", &crate::store::Op::Move { mailbox: "INBOX".into(), uid: 102, dest: "Archive".into() }, i64::MAX, 3).unwrap();
            assert!(st.hide_local(id, op).unwrap());
            assert_eq!(st.ops("a", i64::MAX).unwrap()[0].at, crate::store::At { validity: 7, message_id: "<102@x>".into() }, "queued with its generation");
            op
        };
        // The server resets the mailbox: generation 8, the same messages at new UIDs.
        let (mut s, _log) = fake::session(|c: &str| {
            let renumbered = |a: String| a.replace("UIDVALIDITY 7", "UIDVALIDITY 8").replace("UID 10", "UID 20").replace("UIDNEXT 104", "UIDNEXT 204");
            inbox_server(c).map(renumbered)
        })
        .await;
        sync_mailbox(&mut s, &store, "a", "INBOX", 50, &|| {}).await.unwrap();
        let st = store.lock().unwrap();
        let shown: Vec<u32> = st.list(&crate::model::View::Mailbox { account: "a".into(), mailbox: "INBOX".into() }, &crate::store::Query::default(), false, 10, &HashSet::new()).unwrap().iter().map(|m| m.uid).collect();
        assert_eq!(sorted(shown), vec![201, 203], "the archived message stays hidden in the new generation");
        // Undo still brings it back: it's held by the same op.
        st.undo_batch(3).unwrap();
        assert_eq!(st.list(&crate::model::View::Mailbox { account: "a".into(), mailbox: "INBOX".into() }, &crate::store::Query::default(), false, 10, &HashSet::new()).unwrap().len(), 3);
        assert!(st.ops("a", i64::MAX).unwrap().iter().all(|q| q.id != op), "its move taken back");
    }

    #[tokio::test]
    async fn a_reset_cut_off_between_chunks_still_hides_messages_waiting_to_move() {
        // Cached in generation 6: one message, archived (its move held for the undo window).
        let store = Mutex::new(Store::memory());
        let op = {
            let st = store.lock().unwrap();
            st.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
            st.set_uidvalidity("a", "INBOX", 6).unwrap();
            let p = mime::parse(b"From: a@x.com\r\nSubject: m350\r\nMessage-ID: <350@x>\r\n\r\n", true).unwrap();
            let id = st.upsert_header("a", "INBOX", 5, 0, 100, 0, &p).unwrap();
            let op = st.enqueue_held("a", &crate::store::Op::Move { mailbox: "INBOX".into(), uid: 5, dest: "Archive".into() }, i64::MAX, 3).unwrap();
            assert!(st.hide_local(id, op).unwrap());
            op
        };
        // The server reset INBOX: generation 7, 300 messages (UIDs 101–400), the archived one
        // at UID 350. The first download comes in two chunks, and the connection drops before
        // the second.
        let server = |hang_up: bool| {
            let inner = mailbox_server(300, 0..=0);
            move |c: &str| {
                if hang_up && c.starts_with("FETCH 1:100 ") {
                    return Some(fake::HANG_UP.to_owned());
                }
                if c == "UID FETCH 201:400 (UID FLAGS)" {
                    return Some((101..=300).map(|seq| format!("* {seq} FETCH (UID {} FLAGS ())\r\n", seq + 100)).collect::<String>() + "TAG OK done\r\n");
                }
                inner(c)
            }
        };
        let shown = |store: &Mutex<Store>| -> Vec<u32> {
            let st = store.lock().unwrap();
            st.list(&crate::model::View::Mailbox { account: "a".into(), mailbox: "INBOX".into() }, &crate::store::Query::default(), false, 500, &HashSet::new()).unwrap().iter().map(|m| m.uid).collect()
        };
        let (mut s, _log) = fake::session(server(true)).await;
        assert!(sync_mailbox(&mut s, &store, "a", "INBOX", 300, &|| {}).await.is_err(), "cut off");
        let after = shown(&store);
        assert_eq!(after.len(), 199, "the first chunk landed");
        assert!(!after.contains(&350), "the archived message stays hidden");
        // Envoy starts again: the cache is in generation 7 now, and the sync goes on from it.
        let (mut s, _log) = fake::session(server(false)).await;
        sync_mailbox(&mut s, &store, "a", "INBOX", 300, &|| {}).await.unwrap();
        assert!(!shown(&store).contains(&350), "still hidden after the restart");
        // And it's still its move's: undo brings it back.
        store.lock().unwrap().undo_batch(3).unwrap();
        assert!(shown(&store).contains(&350), "{op}");
    }

    fn sorted(mut v: Vec<u32>) -> Vec<u32> {
        v.sort_unstable();
        v
    }

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
        expunge(&mut s, caps(false), "Trash", &at(5)).await.unwrap();
        move_message(&mut s, caps(false), "INBOX", &at(6), "Archive").await.unwrap();
        let cmds = commands(&log);
        assert!(!expunges_everything(&cmds), "a plain EXPUNGE would destroy everything else flagged \\Deleted: {cmds:?}");
        assert!(cmds.contains(&"UID STORE 5 +FLAGS.SILENT (\\Deleted)".to_owned()), "{cmds:?}");
        assert!(cmds.contains(&"UID COPY 6 \"Archive\"".to_owned()) && cmds.contains(&"UID STORE 6 +FLAGS.SILENT (\\Deleted)".to_owned()), "{cmds:?}");
    }

    #[tokio::test]
    async fn with_uidplus_only_the_given_messages_are_expunged() {
        let (mut s, log) = fake::session(|c| c.starts_with("SELECT").then(|| fake::selected(7, 10))).await;
        expunge(&mut s, caps(true), "Trash", &at(5)).await.unwrap();
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

    // ── Snoozes ──

    const BUCKET: &str = "Snoozed/20261110T0800Z";

    /// A server whose snooze bucket holds UIDs 5 and 6 (`<a@x>`, `<b@x>`; `exists` messages in
    /// all, some maybe flagged \Deleted: the search finds `undeleted`), keeping keywords if
    /// `keywords`.
    fn bucket_server(exists: u32, undeleted: &'static str, keywords: bool) -> impl Fn(&str) -> Option<String> + Send + 'static {
        move |c: &str| {
            let permanent = if keywords { "\\Seen \\Deleted \\*" } else { "\\Seen \\Deleted" };
            Some(match c {
                c if c == format!("SELECT \"{BUCKET}\"") => {
                    format!("* FLAGS (\\Seen \\Deleted)\r\n* {exists} EXISTS\r\n* OK [UIDVALIDITY 9] ok\r\n* OK [PERMANENTFLAGS ({permanent})] ok\r\nTAG OK [READ-WRITE] done\r\n")
                }
                "SELECT \"INBOX\"" => fake::selected(7, 42),
                "UID SEARCH UNDELETED" => format!("* SEARCH {undeleted}\r\nTAG OK done\r\n"),
                c if c.starts_with("UID FETCH 5:6 ") => fake::message_ids(&[(5, "<a@x>"), (6, "<b@x>")]),
                _ => return None,
            })
        }
    }

    fn moves(uidplus: bool) -> Caps {
        Caps { moves: true, idle: false, uidplus }
    }

    #[tokio::test]
    async fn waking_a_bucket_moves_its_messages_back_unread_and_new() {
        let (mut s, log) = fake::session(bucket_server(2, "5 6", true)).await;
        let woken = wake_bucket(&mut s, moves(true), BUCKET, "INBOX").await.unwrap();
        assert_eq!(woken, Woken { moved: 2, message_ids: vec!["<a@x>".into(), "<b@x>".into()] });
        assert_eq!(
            commands(&log),
            [
                format!("SELECT \"{BUCKET}\""),
                "UID SEARCH UNDELETED".into(),
                "UID FETCH 5:6 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into(),
                "UID STORE 5:6 -FLAGS.SILENT (\\Seen)".into(),
                "UID STORE 5:6 +FLAGS.SILENT ($new)".into(),
                "UID MOVE 5:6 \"INBOX\"".into(),
            ]
        );
    }

    #[tokio::test]
    async fn waking_an_emptied_bucket_again_does_nothing() {
        // Another Envoy (or this one) woke it already: only the SELECT.
        let (mut s, log) = fake::session(bucket_server(0, "", true)).await;
        assert_eq!(wake_bucket(&mut s, moves(true), BUCKET, "INBOX").await.unwrap(), Woken::default());
        assert_eq!(commands(&log), [format!("SELECT \"{BUCKET}\"")]);
        // Woken without UIDPLUS: what's left is flagged \Deleted, and not woken twice.
        let (mut s, log) = fake::session(bucket_server(2, "", true)).await;
        assert_eq!(wake_bucket(&mut s, caps(false), BUCKET, "INBOX").await.unwrap(), Woken::default());
        assert_eq!(commands(&log), [format!("SELECT \"{BUCKET}\""), "UID SEARCH UNDELETED".into()]);
    }

    #[tokio::test]
    async fn a_server_without_keywords_wakes_without_new() {
        let (mut s, log) = fake::session(bucket_server(2, "5 6", false)).await;
        wake_bucket(&mut s, moves(true), BUCKET, "INBOX").await.unwrap();
        assert!(!commands(&log).iter().any(|c| c.contains("$new")), "{:?}", commands(&log));
    }

    #[tokio::test]
    async fn without_move_a_wake_copies_then_deletes_exactly_its_originals() {
        // INBOX already has a message with <a@x>'s Message-ID (a copy another Envoy made at
        // the same moment, or a distinct message that shares the ID): it's never touched.
        let (mut s, log) = fake::session(bucket_server(2, "5 6", true)).await;
        let woken = wake_bucket(&mut s, caps(true), BUCKET, "INBOX").await.unwrap();
        assert_eq!(woken.moved, 2);
        assert_eq!(
            commands(&log)[3..],
            [
                "UID STORE 5:6 -FLAGS.SILENT (\\Seen)",
                "UID STORE 5:6 +FLAGS.SILENT ($new)",
                "UID COPY 5:6 \"INBOX\"",
                "UID STORE 5,6 +FLAGS.SILENT (\\Deleted)",
                "UID EXPUNGE 5,6",
            ],
            "nothing in the inbox is deleted, whatever its Message-ID"
        );
    }

    #[tokio::test]
    async fn without_move_or_uidplus_a_wake_never_expunges_nor_touches_the_inbox() {
        let (mut s, log) = fake::session(bucket_server(2, "5 6", true)).await;
        wake_bucket(&mut s, caps(false), BUCKET, "INBOX").await.unwrap();
        assert_eq!(commands(&log)[3..], ["UID STORE 5:6 -FLAGS.SILENT (\\Seen)", "UID STORE 5:6 +FLAGS.SILENT ($new)", "UID COPY 5:6 \"INBOX\"", "UID STORE 5,6 +FLAGS.SILENT (\\Deleted)"]);
    }

    #[tokio::test]
    async fn a_wake_cut_short_is_lost_not_done() {
        for how in BREAKS {
            let inner = bucket_server(2, "5 6", true);
            let (mut s, log) = fake::session(move |c: &str| if c.starts_with("UID MOVE") { Some(broken("* 1 EXPUNGE\r\n", how)) } else { inner(c) }).await;
            assert!(wake_bucket(&mut s, moves(true), BUCKET, "INBOX").await.is_err(), "{how:?}");
            assert!(commands(&log).iter().any(|c| c.starts_with("UID MOVE")));
        }
    }

    #[tokio::test]
    async fn an_empty_bucket_is_deleted_and_unsubscribed_one_with_mail_never() {
        let status = |messages: u32| move |c: &str| match c {
            c if c.starts_with("STATUS") => Some(format!("* STATUS \"{BUCKET}\" (MESSAGES {messages} UNSEEN 0)\r\nTAG OK done\r\n")),
            c if c.starts_with("SELECT") => Some(fake::selected(7, 10)),
            _ => None,
        };
        let (mut s, log) = fake::session(status(0)).await;
        assert!(cleanup_bucket(&mut s, BUCKET, "INBOX").await.unwrap());
        // The STATUS is right before the DELETE: nothing else between them.
        assert_eq!(commands(&log), ["SELECT \"INBOX\"".to_owned(), format!("STATUS \"{BUCKET}\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)"), format!("DELETE \"{BUCKET}\""), format!("UNSUBSCRIBE \"{BUCKET}\"")]);
        let (mut s, log) = fake::session(status(1)).await;
        assert!(!cleanup_bucket(&mut s, BUCKET, "INBOX").await.unwrap());
        assert!(!commands(&log).iter().any(|c| c.starts_with("DELETE")));
        // Gone already: nothing to delete.
        let (mut s, log) = fake::session(|c: &str| match c {
            c if c.starts_with("SELECT") => Some(fake::selected(7, 10)),
            c if c.starts_with("STATUS") => Some("TAG NO [NONEXISTENT] no such mailbox\r\n".to_owned()),
            _ => None,
        })
        .await;
        assert!(cleanup_bucket(&mut s, BUCKET, "INBOX").await.unwrap());
        assert!(!commands(&log).iter().any(|c| c.starts_with("DELETE")));
        // A STATUS cut short deletes nothing.
        for how in BREAKS.into_iter().filter(|h| !matches!(h, Breaks::No | Breaks::Bad)) {
            let (mut s, log) = fake::session(move |c: &str| match c {
                c if c.starts_with("SELECT") => Some(fake::selected(7, 10)),
                c if c.starts_with("STATUS") => Some(broken(&format!("* STATUS \"{BUCKET}\" (MESSAGES 0 UNSEEN 0)\r\n"), how)),
                _ => None,
            })
            .await;
            assert!(cleanup_bucket(&mut s, BUCKET, "INBOX").await.is_err());
            assert!(!commands(&log).iter().any(|c| c.starts_with("DELETE")), "{how:?}");
        }
    }

    #[tokio::test]
    async fn a_bucket_is_made_under_its_parent_and_subscribed() {
        let (mut s, log) = fake::session(|_| None).await;
        ensure_bucket(&mut s, "Snoozed", BUCKET).await.unwrap();
        assert_eq!(commands(&log), ["CREATE \"Snoozed\"".to_owned(), format!("CREATE \"{BUCKET}\""), "SUBSCRIBE \"Snoozed\"".into(), format!("SUBSCRIBE \"{BUCKET}\"")]);
        // Already there (said with ALREADYEXISTS, or not), and subscriptions refused: fine.
        let (mut s, _log) = fake::session(|c: &str| match c {
            c if c.starts_with("CREATE") => Some("TAG NO [ALREADYEXISTS] exists\r\n".into()),
            c if c.starts_with("LIST") => Some(format!("* LIST () \"/\" {}\r\nTAG OK done\r\n", &c["LIST \"\" ".len()..])),
            c if c.starts_with("SUBSCRIBE") => Some("TAG NO not supported\r\n".into()),
            _ => None,
        })
        .await;
        ensure_bucket(&mut s, "Snoozed", BUCKET).await.unwrap();
        // Refused, and not there: the parent can't be had.
        let (mut s, log) = fake::session(|c: &str| c.starts_with("CREATE").then(|| "TAG NO [CANNOT] reserved name\r\n".to_owned())).await;
        let e = ensure_bucket(&mut s, "Snoozed", BUCKET).await.unwrap_err();
        assert_eq!(e.downcast_ref::<CantCreate>().map(|c| c.mailbox.as_str()), Some("Snoozed"), "{e:#}");
        assert!(!commands(&log).iter().any(|c| c.contains(BUCKET)), "nothing under a parent that isn't there");
    }

    #[tokio::test]
    async fn permanentflags_say_whether_new_can_be_kept() {
        let answer = |permanent: &'static str| move |c: &str| c.starts_with("SELECT").then(|| format!("* FLAGS (\\Seen)\r\n* 1 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n{permanent}TAG OK done\r\n"));
        for (permanent, keywords) in [
            ("* OK [PERMANENTFLAGS (\\Seen \\Deleted \\*)] ok\r\n", true),
            ("* OK [PERMANENTFLAGS (\\Seen $New)] ok\r\n", true),
            ("* OK [PERMANENTFLAGS (\\Seen \\Deleted $MDNSent)] ok\r\n", false),
            ("", false),
        ] {
            let (mut s, _log) = fake::session(answer(permanent)).await;
            assert_eq!(wire::select(&mut s, "INBOX").await.unwrap().keywords, keywords, "{permanent:?}");
        }
        // Where it can't, a queued change leaves `$new` out (it's this computer's mark there).
        let (mut s, log) = fake::session(answer("* OK [PERMANENTFLAGS (\\Seen)] ok\r\n")).await;
        store_flags(&mut s, "INBOX", &at(1), flags::WOKEN, flags::SEEN).await.unwrap();
        store_flags(&mut s, "INBOX", &at(1), 0, flags::WOKEN).await.unwrap();
        assert_eq!(commands(&log).into_iter().filter(|c| c.starts_with("UID STORE")).collect::<Vec<_>>(), ["UID STORE 1 -FLAGS.SILENT (\\Seen)"]);
        let (mut s, log) = fake::session(answer("* OK [PERMANENTFLAGS (\\*)] ok\r\n")).await;
        store_flags(&mut s, "INBOX", &at(1), 0, flags::WOKEN).await.unwrap();
        assert_eq!(commands(&log).last().map(String::as_str), Some("UID STORE 1 -FLAGS.SILENT ($new)"));
    }

    #[test]
    fn new_is_the_woken_mark() {
        assert_eq!(flag_bits([Flag::Seen, Flag::Custom("$New".into()), Flag::Custom("$Junk".into())].into_iter()), flags::SEEN | flags::WOKEN);
        assert_eq!(flag_names(flags::SEEN | flags::WOKEN), "\\Seen $new");
    }

    #[tokio::test]
    async fn buckets_their_parents_and_the_servers_own_snoozed_are_listed_as_snoozed() {
        let listing = "* LIST (\\HasNoChildren) \"/\" INBOX\r\n* LIST (\\HasChildren) \"/\" Snoozed\r\n* LIST (\\HasNoChildren) \"/\" \"Snoozed/20261110T0800Z\"\r\n\
                       * LIST (\\HasNoChildren) \"/\" \"Envoy Snoozed/20261111T0800Z\"\r\n* LIST (\\HasNoChildren) \"/\" Work\r\n* LIST (\\HasNoChildren \\Archive) \"/\" Archive\r\nTAG OK done\r\n";
        let (mut s, _log) = fake::session(move |c: &str| c.starts_with("LIST").then(|| listing.to_owned())).await;
        let listed = list_mailboxes(&mut s).await.unwrap();
        let roles: Vec<(&str, Role, &str)> = listed.iter().map(|m| (m.name.as_str(), m.role, m.display.as_str())).collect();
        assert_eq!(
            roles,
            [
                ("INBOX", Role::Inbox, "Inbox"),
                ("Snoozed", Role::Snoozed, "Snoozed"),
                ("Snoozed/20261110T0800Z", Role::Snoozed, "Snoozed"),
                ("Envoy Snoozed/20261111T0800Z", Role::Snoozed, "Snoozed"),
                ("Work", Role::Other, "Work"),
                ("Archive", Role::Archive, "Archive"),
            ]
        );
        assert_eq!(snooze_parent(&listed).name, "Snoozed");
        // The server's own: shown as snoozed, and Envoy's go beside it.
        let native = "* LIST (\\HasNoChildren) \"/\" INBOX\r\n* LIST (\\Snoozed) \"/\" Snoozed\r\nTAG OK done\r\n";
        let (mut s, _log) = fake::session(move |c: &str| c.starts_with("LIST").then(|| native.to_owned())).await;
        let listed = list_mailboxes(&mut s).await.unwrap();
        assert!(listed[1].native_snoozed && listed[1].role == Role::Snoozed);
        assert_eq!(snooze_parent(&listed).name, "Envoy Snoozed");
    }

    /// What a LIST of `names` (with `delimiter`; `(name, attributes)`) gives the Snoozed role.
    async fn snoozed_of(delimiter: &str, names: &[(&str, &str)]) -> Vec<String> {
        let mut listing: String = names.iter().map(|(n, attrs)| format!("* LIST ({attrs}) \"{delimiter}\" \"{n}\"\r\n")).collect();
        listing.push_str("TAG OK done\r\n");
        let (mut s, _log) = fake::session(move |c: &str| c.starts_with("LIST").then(|| listing.clone())).await;
        list_mailboxes(&mut s).await.unwrap().into_iter().filter(|m| m.role == Role::Snoozed).map(|m| m.name).collect()
    }

    #[tokio::test]
    async fn a_bucket_is_its_parents_child_by_the_servers_own_delimiter() {
        // On a `/` server: only `Snoozed/<stamp>` (and `Envoy Snoozed/<stamp>`) are buckets.
        let found = snoozed_of(
            "/",
            &[
                ("INBOX", "\\HasNoChildren"),
                ("Snoozed", "\\HasChildren"),
                ("Snoozed/20261110T0800Z", ""),
                ("Envoy Snoozed/20261111T0800Z", ""),
                ("Snoozed-20261110T0800Z", ""),
                ("Snoozed.20261110T0800Z", ""),
                ("Work/Snoozed/20261110T0800Z", ""),
                ("Archive/Envoy Snoozed", ""),
            ],
        )
        .await;
        assert_eq!(found, ["Snoozed", "Snoozed/20261110T0800Z", "Envoy Snoozed/20261111T0800Z"]);
        // On a `.` server with folders inside INBOX: under `INBOX.Snoozed`, by `.`.
        let found = snoozed_of(".", &[("INBOX", ""), ("INBOX.Sent", ""), ("INBOX.Snoozed.20261110T0800Z", ""), ("INBOX.Snoozed/20261110T0800Z", ""), ("INBOX.Work.Snoozed.20261110T0800Z", "")]).await;
        assert_eq!(found, ["INBOX.Snoozed.20261110T0800Z"]);
        // The server's own snoozed mailbox is known by its attribute, whatever its name; a
        // `Snoozed` without it is Envoy's parent, nothing more.
        let found = snoozed_of("/", &[("INBOX", ""), ("Later", "\\Snoozed"), ("Work", "")]).await;
        assert_eq!(found, ["Later"]);
    }

    #[test]
    fn uid_sets_fold_runs() {
        assert_eq!(uid_set(&[3, 4, 5, 8, 10, 11]), "3:5,8,10:11");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[]), "");
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
