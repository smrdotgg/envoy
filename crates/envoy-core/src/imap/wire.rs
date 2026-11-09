//! The commands whose answers Envoy acts on, read to the end: every untagged response up to the
//! command's tagged OK.
//!
//! async-imap 0.10 ends an answer early and returns what came so far as success when the
//! connection closes (EOF), and, for FETCH, SEARCH, LIST, STORE and EXPUNGE, when the server
//! finishes with a tagged NO or BAD: a SELECT reads as an empty mailbox with no UIDVALIDITY, a
//! flag listing as one missing messages, a LIST as one missing mailboxes, a STORE as done. A
//! sync acting on that deletes cached mail, and a replay reports a change the server never
//! made. Here an answer counts only once its tagged OK has arrived. A tagged NO or BAD is that
//! refusal, as async-imap's error (so [`super::failure`] reads it as before); the connection
//! closing, a BYE, or the time limit passing first is a lost connection ([`Lost`]). Nothing is
//! returned from an answer that didn't finish.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_imap::imap_proto::{AttributeValue, MailboxDatum, MessageSection, NameAttribute, Response, ResponseCode, SectionPath, Status, StatusAttribute};
use async_imap::types::Flag;

use super::{COMMAND_TIMEOUT, Connection, FETCH_TIMEOUT, Io, flag_bits, quoted};

/// The connection went before the server finished answering: closed (inside a literal too),
/// broken, garbled, BYE, or silent past the time limit. Nothing about the answer is known, and
/// the connection is done with ([`Connection`]).
#[derive(Debug)]
pub struct Lost(pub String);

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (connection lost)", self.0)
    }
}

impl std::error::Error for Lost {}

/// A tagged NO or BAD, as async-imap reports one.
pub(super) fn refused(status: &Status, code: &impl std::fmt::Debug, info: &impl std::fmt::Debug) -> anyhow::Error {
    let what = format!("code: {code:?}, info: {info:?}");
    anyhow::Error::new(if *status == Status::Bad { async_imap::error::Error::Bad(what) } else { async_imap::error::Error::No(what) })
}

/// Sends `command` and reads its whole answer, up to its tagged OK, handing each untagged
/// response to `each` as it comes. `each` only collects: what it collected may be used only if
/// this returns Ok (the answer finished). `what` names the command in errors; `limit` bounds
/// the whole exchange.
///
/// A connection whose earlier exchange didn't finish is never used ([`Connection`]); one
/// whose exchange here doesn't finish is poisoned. Every failure but the server's refusal is
/// [`Lost`]: a write that fails, the connection closing (inside a literal too: async-imap's
/// "bytes remaining in stream"), a garbled answer, a BYE, the time limit.
pub(super) async fn run(session: &mut Connection<impl Io>, command: &str, what: &str, limit: Duration, mut each: impl FnMut(&Response<'_>) + Send) -> Result<()> {
    session.usable()?;
    let lost = |why: String| anyhow::Error::new(Lost(format!("{what}: {why}")));
    let inner = &mut session.inner;
    let exchange = async {
        let tag = inner.run_command(command).await.map_err(|e| lost(e.to_string()))?;
        loop {
            let Some(next) = inner.read_response().await else {
                return Err(lost("the connection closed before the server finished".into()));
            };
            let response = next.map_err(|e| lost(e.to_string()))?;
            match response.parsed() {
                Response::Done { tag: t, status, code, information } if *t == tag => {
                    return match status {
                        Status::Ok => Ok(()),
                        Status::No | Status::Bad => Err(refused(status, code, information)),
                        other => Err(lost(format!("the server ended with {other:?}"))),
                    };
                }
                Response::Data { status: Status::Bye, information, .. } => {
                    return Err(lost(format!("the server said BYE ({})", information.as_deref().unwrap_or(""))));
                }
                // An earlier command's end (one that timed out, say): not this answer's.
                Response::Done { .. } => {}
                other => each(other),
            }
        }
    };
    let r = tokio::time::timeout(limit, exchange).await.unwrap_or_else(|_| Err(lost("timed out".into())));
    session.finished(r)
}

/// Runs a command whose answer is just its tagged OK (STORE, UID EXPUNGE, COPY, MOVE).
pub(super) async fn ok(session: &mut Connection<impl Io>, command: &str, what: &str) -> Result<()> {
    ok_within(session, command, what, COMMAND_TIMEOUT).await
}

/// [`ok`], within `limit`.
pub(super) async fn ok_within(session: &mut Connection<impl Io>, command: &str, what: &str, limit: Duration) -> Result<()> {
    run(session, command, what, limit, |_| {}).await
}

/// What a finished SELECT said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Selected {
    pub exists: u32,
    pub uid_validity: u32,
    pub uid_next: Option<u32>,
    /// It also told of messages expunged (by another client, as it answered).
    pub expunged: bool,
}

/// SELECTs `mailbox`. Every SELECT says FLAGS, EXISTS and UIDVALIDITY (RFC 3501 requires all
/// three): an answer without one of them, even finished, isn't trusted to describe the mailbox.
pub(super) async fn select(session: &mut Connection<impl Io>, mailbox: &str) -> Result<Selected> {
    let (mut flags, mut exists, mut uid_validity, mut uid_next, mut expunged) = (false, None, None, None, false);
    run(session, &format!("SELECT {}", quoted(mailbox)), "SELECT", COMMAND_TIMEOUT, |r| match r {
        Response::MailboxData(MailboxDatum::Flags(_)) => flags = true,
        Response::MailboxData(MailboxDatum::Exists(n)) => exists = Some(*n),
        Response::Data { status: Status::Ok, code: Some(ResponseCode::UidValidity(v)), .. } => uid_validity = Some(*v),
        Response::Data { status: Status::Ok, code: Some(ResponseCode::UidNext(n)), .. } => uid_next = Some(*n),
        Response::Expunge(_) => expunged = true,
        _ => {}
    })
    .await?;
    let missing: Vec<&str> = [(flags, "FLAGS"), (exists.is_some(), "EXISTS"), (uid_validity.is_some(), "UIDVALIDITY")].into_iter().filter(|(had, _)| !had).map(|(_, name)| name).collect();
    match (exists, uid_validity) {
        (Some(exists), Some(uid_validity)) if flags => Ok(Selected { exists, uid_validity, uid_next, expunged }),
        _ => bail!("SELECT {mailbox}: the server's answer didn't say {}", missing.join(", ")),
    }
}

/// `(unseen, messages)` of `mailbox`, each as the server said it: an answer missing either
/// isn't read as zero.
pub(super) async fn status(session: &mut Connection<impl Io>, mailbox: &str) -> Result<(u32, u32)> {
    let (mut unseen, mut messages) = (None, None);
    run(session, &format!("STATUS {} (MESSAGES UNSEEN)", quoted(mailbox)), "STATUS", COMMAND_TIMEOUT, |r| {
        if let Response::MailboxData(MailboxDatum::Status { mailbox: m, status }) = r
            && (m.as_ref() == mailbox || (m.eq_ignore_ascii_case("INBOX") && mailbox.eq_ignore_ascii_case("INBOX")))
        {
            for item in status {
                match item {
                    StatusAttribute::Unseen(n) => unseen = Some(*n),
                    StatusAttribute::Messages(n) => messages = Some(*n),
                    _ => {}
                }
            }
        }
    })
    .await?;
    match (unseen, messages) {
        (Some(unseen), Some(messages)) => Ok((unseen, messages)),
        _ => Err(anyhow!("STATUS {mailbox}: the server's answer didn't say {}", if unseen.is_none() && messages.is_none() { "MESSAGES, UNSEEN" } else if unseen.is_none() { "UNSEEN" } else { "MESSAGES" })),
    }
}

/// A mailbox a finished LIST named.
#[derive(Debug)]
pub(super) struct Listed {
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<Attribute>,
}

/// The LIST attributes Envoy reads.
#[derive(Debug, PartialEq)]
pub(super) enum Attribute {
    NoSelect,
    All,
    Archive,
    Drafts,
    Flagged,
    Junk,
    Sent,
    Trash,
    /// Any other, lowercased (`\important`, `\nonexistent`).
    Other(String),
}

/// Every mailbox (`LIST "" "*"`).
pub(super) async fn list(session: &mut Connection<impl Io>) -> Result<Vec<Listed>> {
    let mut listed = Vec::new();
    run(session, "LIST \"\" \"*\"", "LIST", COMMAND_TIMEOUT, |r| {
        if let Response::MailboxData(MailboxDatum::List { name_attributes, delimiter, name }) = r {
            listed.push(Listed {
                name: name.to_string(),
                delimiter: delimiter.as_ref().map(|d| d.to_string()),
                attributes: name_attributes
                    .iter()
                    .map(|a| match a {
                        NameAttribute::NoSelect => Attribute::NoSelect,
                        NameAttribute::All => Attribute::All,
                        NameAttribute::Archive => Attribute::Archive,
                        NameAttribute::Drafts => Attribute::Drafts,
                        NameAttribute::Flagged => Attribute::Flagged,
                        NameAttribute::Junk => Attribute::Junk,
                        NameAttribute::Sent => Attribute::Sent,
                        NameAttribute::Trash => Attribute::Trash,
                        NameAttribute::Extension(s) => Attribute::Other(s.to_ascii_lowercase()),
                        other => Attribute::Other(format!("{other:?}").to_ascii_lowercase()),
                    })
                    .collect(),
            });
        }
    })
    .await?;
    Ok(listed)
}

/// One message as a finished FETCH told of it.
#[derive(Debug, Default)]
pub struct Fetched {
    pub uid: Option<u32>,
    /// FLAGS, when the response had them.
    pub flags: Option<u32>,
    pub size: Option<u32>,
    /// INTERNALDATE, Unix seconds.
    pub internal_date: Option<i64>,
    /// `BODY[HEADER…]` (the header fields asked for) or `RFC822.HEADER`.
    pub header: Option<Vec<u8>>,
    /// `BODY[]` or `RFC822`: the whole message.
    pub body: Option<Vec<u8>>,
}

impl Fetched {
    fn of(attributes: &[AttributeValue<'_>]) -> Fetched {
        let mut f = Fetched::default();
        for a in attributes {
            match a {
                AttributeValue::Uid(uid) => f.uid = Some(*uid),
                AttributeValue::Flags(names) => f.flags = Some(flag_bits(names.iter().map(|n| Flag::from(n.as_ref())))),
                AttributeValue::Rfc822Size(size) => f.size = Some(*size),
                AttributeValue::InternalDate(d) => f.internal_date = chrono::DateTime::parse_from_str(d, "%d-%b-%Y %H:%M:%S %z").ok().map(|d| d.timestamp()),
                AttributeValue::BodySection { section: Some(SectionPath::Full(MessageSection::Header)), data: Some(h), .. } | AttributeValue::Rfc822Header(Some(h)) => {
                    f.header = Some(h.to_vec());
                }
                AttributeValue::BodySection { section: None, data: Some(b), .. } | AttributeValue::Rfc822(Some(b)) => f.body = Some(b.to_vec()),
                _ => {}
            }
        }
        f
    }
}

/// FETCH (`uid`: UID FETCH) of `set`, every response it gave, in order, including ones the
/// server sent unasked meanwhile (another client's flag changes). Bodies get the longer limit.
pub(super) async fn fetch(session: &mut Connection<impl Io>, uid: bool, set: &str, query: &str, what: &str) -> Result<Vec<Fetched>> {
    let command = format!("{}FETCH {set} {query}", if uid { "UID " } else { "" });
    let mut fetched = Vec::new();
    run(session, &command, what, FETCH_TIMEOUT, |r| {
        if let Response::Fetch(_, attributes) = r {
            fetched.push(Fetched::of(attributes));
        }
    })
    .await?;
    Ok(fetched)
}

/// The messages in a UID range and their flags, as a finished UID FETCH listed them: the only
/// listing [`crate::store::Store::reconcile`] takes, so a cached message is deleted as gone
/// only when an answer the server finished left it out. Made only here, by [`uid_flags`].
#[derive(Debug)]
pub struct ServerFlags {
    flags: std::collections::HashMap<u32, u32>,
}

impl ServerFlags {
    /// The message's flags, if the listing had it.
    pub fn get(&self, uid: u32) -> Option<u32> {
        self.flags.get(&uid).copied()
    }

    /// A listing for tests, as if a FETCH had finished with it.
    #[cfg(test)]
    pub fn finished(flags: impl IntoIterator<Item = (u32, u32)>) -> ServerFlags {
        ServerFlags { flags: flags.into_iter().collect() }
    }
}

/// UID FETCH of `lo:hi`'s flags. A message without FLAGS in the answer (unasked news of another
/// one, say) still counts as there.
pub(super) async fn uid_flags(session: &mut Connection<impl Io>, lo: u32, hi: u32) -> Result<ServerFlags> {
    let fetched = fetch(session, true, &format!("{lo}:{hi}"), "(UID FLAGS)", "FETCH flags").await?;
    Ok(ServerFlags { flags: fetched.iter().filter_map(|f| Some((f.uid?, f.flags.unwrap_or(0)))).collect() })
}

/// UID SEARCH: the UIDs a finished search found.
pub(super) async fn uid_search(session: &mut Connection<impl Io>, query: &str) -> Result<HashSet<u32>> {
    let mut found = HashSet::new();
    run(session, &format!("UID SEARCH {query}"), "SEARCH", COMMAND_TIMEOUT, |r| {
        if let Response::MailboxData(MailboxDatum::Search(uids)) = r {
            found.extend(uids.iter().copied());
        }
    })
    .await?;
    Ok(found)
}

/// CAPABILITY: the capabilities' names, uppercased.
pub(super) async fn capabilities(session: &mut Connection<impl Io>) -> Result<HashSet<String>> {
    use async_imap::imap_proto::Capability;
    let mut names = HashSet::new();
    run(session, "CAPABILITY", "CAPABILITY", COMMAND_TIMEOUT, |r| {
        if let Response::Capabilities(caps) = r {
            names.extend(caps.iter().map(|c| match c {
                Capability::Imap4rev1 => "IMAP4REV1".to_owned(),
                Capability::Auth(a) => format!("AUTH={}", a.to_ascii_uppercase()),
                Capability::Atom(a) => a.to_ascii_uppercase(),
            }));
        }
    })
    .await?;
    Ok(names)
}
