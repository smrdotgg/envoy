//! The local mail cache: SQLite in WAL mode, with FTS5 for instant search.
//!
//! The UI never waits on the network: it reads lists and bodies from here, and every action
//! (read, star, archive, delete, send) is applied here first and queued in `ops` for the sync
//! worker to replay on the server, retrying until it succeeds.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};

use crate::mime::{self, Parsed};
use crate::model::{Address, Attachment, Body, Mailbox, MessageId, Role, Summary, ThreadMessage, View, flags};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS mailboxes (
    account TEXT NOT NULL,
    name TEXT NOT NULL,
    display TEXT NOT NULL,
    role TEXT NOT NULL,
    delimiter TEXT NOT NULL DEFAULT '/',
    selectable INTEGER NOT NULL DEFAULT 1,
    uidvalidity INTEGER NOT NULL DEFAULT 0,
    unread INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0,
    synced INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account, name)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    uid INTEGER NOT NULL,
    message_id TEXT NOT NULL DEFAULT '',
    thread TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    from_name TEXT NOT NULL DEFAULT '',
    from_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '[]',
    date INTEGER NOT NULL DEFAULT 0,
    flags INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    preview TEXT NOT NULL DEFAULT '',
    has_attachments INTEGER NOT NULL DEFAULT 0,
    has_body INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account, mailbox, uid)
);
CREATE INDEX IF NOT EXISTS messages_box_date ON messages (account, mailbox, date DESC);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (account, thread);
CREATE INDEX IF NOT EXISTS messages_flagged ON messages (flags) WHERE flags & 2;
CREATE TABLE IF NOT EXISTS bodies (
    id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    raw BLOB NOT NULL,
    text TEXT,
    html TEXT,
    markdown TEXT,
    cc TEXT NOT NULL DEFAULT '[]',
    reply_to TEXT NOT NULL DEFAULT '[]',
    refs TEXT NOT NULL DEFAULT '',
    attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5 (subject, people, body, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS ops (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    op TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    not_before INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM fts WHERE rowid = old.id;
END;
"#;

/// A change queued for the server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Op {
    SetFlags { mailbox: String, uid: u32, add: u32, remove: u32 },
    Move { mailbox: String, uid: u32, dest: String },
    /// Delete for good (expunge), used in Trash.
    Expunge { mailbox: String, uid: u32 },
    Send { raw_b64: String, from: String, to: Vec<String>, sent_mailbox: Option<String>, reply_of: Option<(String, u32)> },
}

pub struct QueuedOp {
    pub id: i64,
    pub op: Op,
    pub attempts: u32,
}

pub struct Store {
    conn: Connection,
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn from_json<T: for<'de> Deserialize<'de> + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

#[derive(Serialize, Deserialize, Default)]
struct AttachmentRow {
    index: usize,
    filename: String,
    mime: String,
    size: usize,
}

/// A parsed search: free text for FTS plus `from:`, `to:`, `subject:`, `is:`, `has:` filters.
#[derive(Default, Debug, PartialEq)]
pub struct Query {
    pub text: Vec<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: Vec<String>,
    pub unread: Option<bool>,
    pub starred: Option<bool>,
    pub attachment: bool,
}

impl Query {
    pub fn parse(q: &str) -> Query {
        let mut out = Query::default();
        for tok in q.split_whitespace() {
            let lower = tok.to_lowercase();
            if let Some(v) = lower.strip_prefix("from:") {
                out.from.push(v.to_owned());
            } else if let Some(v) = lower.strip_prefix("to:") {
                out.to.push(v.to_owned());
            } else if let Some(v) = lower.strip_prefix("subject:") {
                out.subject.push(v.to_owned());
            } else if lower == "is:unread" {
                out.unread = Some(true);
            } else if lower == "is:read" {
                out.unread = Some(false);
            } else if lower == "is:starred" || lower == "is:flagged" {
                out.starred = Some(true);
            } else if lower == "has:attachment" || lower == "has:attachments" {
                out.attachment = true;
            } else {
                out.text.push(tok.to_owned());
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }
}

fn fts_term(t: &str) -> String {
    let clean: String = t.chars().filter(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '\'')).collect();
    format!("\"{}\"*", clean.replace('"', ""))
}

const SUMMARY_COLS: &str = "m.id, m.account, m.mailbox, m.uid, m.thread, m.subject, m.from_name, m.from_email, m.recipients, m.date, m.flags, m.preview, m.has_attachments, m.message_id";

fn summary_row(r: &rusqlite::Row) -> rusqlite::Result<(Summary, String)> {
    let recipients: String = r.get(8)?;
    Ok((
        Summary {
            id: r.get(0)?,
            account: r.get(1)?,
            mailbox: r.get(2)?,
            uid: r.get(3)?,
            thread: r.get(4)?,
            subject: r.get(5)?,
            from: Address { name: r.get(6)?, email: r.get(7)? },
            to: from_json(&recipients),
            date: r.get(9)?,
            flags: r.get(10)?,
            preview: r.get(11)?,
            has_attachments: r.get::<_, i64>(12)? != 0,
            thread_count: 1,
            participants: String::new(),
            thread_unread: false,
        },
        r.get(13)?,
    ))
}

fn first_name(a: &Address, me: &HashSet<String>) -> String {
    if me.contains(&a.email.to_lowercase()) {
        return "me".into();
    }
    let d = a.display();
    let d = d.split('@').next().unwrap_or(d);
    d.split_whitespace().next().unwrap_or(d).trim_matches(|c: char| c == '"' || c == '\'').to_owned()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Store { conn }
    }

    // ── mailboxes ──

    pub fn upsert_mailbox(&self, account: &str, name: &str, display: &str, role: Role, delimiter: &str, selectable: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mailboxes (account, name, display, role, delimiter, selectable) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account, name) DO UPDATE SET display = ?3, role = ?4, delimiter = ?5, selectable = ?6",
            params![account, name, display, role.as_str(), delimiter, selectable],
        )?;
        Ok(())
    }

    /// Drops mailboxes the server no longer lists (and their messages).
    pub fn retain_mailboxes(&self, account: &str, names: &[String]) -> Result<()> {
        let existing: Vec<String> = self
            .conn
            .prepare("SELECT name FROM mailboxes WHERE account = ?1")?
            .query_map(params![account], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for name in existing.iter().filter(|n| !names.contains(n)) {
            self.clear_mailbox(account, name)?;
            self.conn.execute("DELETE FROM mailboxes WHERE account = ?1 AND name = ?2", params![account, name])?;
        }
        Ok(())
    }

    pub fn set_counts(&self, account: &str, name: &str, unread: u32, total: u32) -> Result<()> {
        self.conn.execute("UPDATE mailboxes SET unread = ?3, total = ?4 WHERE account = ?1 AND name = ?2", params![account, name, unread, total])?;
        Ok(())
    }

    pub fn mark_synced(&self, account: &str, name: &str) -> Result<()> {
        self.conn.execute("UPDATE mailboxes SET synced = 1 WHERE account = ?1 AND name = ?2", params![account, name])?;
        Ok(())
    }

    pub fn uidvalidity(&self, account: &str, name: &str) -> Result<u32> {
        Ok(self
            .conn
            .query_row("SELECT uidvalidity FROM mailboxes WHERE account = ?1 AND name = ?2", params![account, name], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_uidvalidity(&self, account: &str, name: &str, v: u32) -> Result<()> {
        self.conn.execute("UPDATE mailboxes SET uidvalidity = ?3 WHERE account = ?1 AND name = ?2", params![account, name, v])?;
        Ok(())
    }

    pub fn mailboxes(&self, account: &str) -> Result<Vec<Mailbox>> {
        let mut stmt =
            self.conn.prepare("SELECT account, name, display, role, delimiter, selectable, unread, total FROM mailboxes WHERE account = ?1")?;
        let mut out: Vec<Mailbox> = stmt
            .query_map(params![account], |r| {
                let name: String = r.get(1)?;
                let delim: String = r.get(4)?;
                let depth = if delim.is_empty() { 0 } else { name.matches(delim.as_str()).count() as u8 };
                Ok(Mailbox {
                    account: r.get(0)?,
                    name,
                    display: r.get(2)?,
                    role: Role::parse(&r.get::<_, String>(3)?),
                    selectable: r.get::<_, i64>(5)? != 0,
                    unread: r.get(6)?,
                    total: r.get(7)?,
                    depth,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.sort_by(|a, b| a.role.rank().cmp(&b.role.rank()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    pub fn mailbox_for_role(&self, account: &str, role: Role) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT name FROM mailboxes WHERE account = ?1 AND role = ?2 LIMIT 1", params![account, role.as_str()], |r| r.get(0))
            .optional()?)
    }

    pub fn role_of(&self, account: &str, mailbox: &str) -> Role {
        self.conn
            .query_row("SELECT role FROM mailboxes WHERE account = ?1 AND name = ?2", params![account, mailbox], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .map_or(Role::Other, |s| Role::parse(&s))
    }

    // ── messages ──

    pub fn clear_mailbox(&self, account: &str, mailbox: &str) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE account = ?1 AND mailbox = ?2", params![account, mailbox])?;
        Ok(())
    }

    pub fn local_uids(&self, account: &str, mailbox: &str) -> Result<Vec<(u32, u32)>> {
        let mut stmt = self.conn.prepare_cached("SELECT uid, flags FROM messages WHERE account = ?1 AND mailbox = ?2 ORDER BY uid")?;
        let rows = stmt.query_map(params![account, mailbox], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn max_uid(&self, account: &str, mailbox: &str) -> Result<u32> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(uid), 0) FROM messages WHERE account = ?1 AND mailbox = ?2", params![account, mailbox], |r| r.get(0))?)
    }

    /// Inserts (or refreshes the flags of) a message from its headers. Returns its id.
    #[allow(clippy::too_many_arguments)] // one per IMAP FETCH item
    pub fn upsert_header(&self, account: &str, mailbox: &str, uid: u32, fl: u32, size: u32, internal_date: i64, p: &Parsed) -> Result<MessageId> {
        let thread = mime::thread_id(&p.message_id, &p.in_reply_to, &p.references);
        let thread = if thread.is_empty() { format!("uid:{mailbox}:{uid}") } else { thread };
        let mut recipients = p.to.clone();
        recipients.extend(p.cc.iter().cloned());
        let date = p.date.filter(|d| *d > 0).unwrap_or(internal_date);
        let id: MessageId = self.conn.query_row(
            "INSERT INTO messages (account, mailbox, uid, message_id, thread, subject, from_name, from_email, recipients, date, flags, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (account, mailbox, uid) DO UPDATE SET flags = ?11
             RETURNING id",
            params![account, mailbox, uid, p.message_id, thread, p.subject, p.from.name, p.from.email, json(&recipients), date, fl, size],
            |r| r.get(0),
        )?;
        let people = format!(
            "{} {} {}",
            p.from.name,
            p.from.email,
            recipients.iter().map(|a| format!("{} {}", a.name, a.email)).collect::<Vec<_>>().join(" ")
        );
        self.conn.execute("DELETE FROM fts WHERE rowid = ?1", params![id])?;
        self.conn.execute("INSERT INTO fts (rowid, subject, people, body) VALUES (?1, ?2, ?3, '')", params![id, p.subject, people])?;
        Ok(id)
    }

    pub fn set_body(&self, id: MessageId, raw: &[u8], p: &Parsed) -> Result<()> {
        let preview = mime::preview(p.text.as_deref(), p.html.as_deref());
        let attachments: Vec<AttachmentRow> =
            p.attachments.iter().map(|a| AttachmentRow { index: a.index, filename: a.filename.clone(), mime: a.mime.clone(), size: a.size }).collect();
        self.conn.execute(
            "INSERT OR REPLACE INTO bodies (id, raw, text, html, markdown, cc, reply_to, refs, attachments) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8)",
            params![id, raw, p.text, p.html, json(&p.cc), json(&p.reply_to), p.references, json(&attachments)],
        )?;
        self.conn.execute(
            "UPDATE messages SET has_body = 1, preview = ?2, has_attachments = ?3 WHERE id = ?1",
            params![id, preview, !attachments.is_empty()],
        )?;
        let body_text = match (&p.text, &p.html) {
            (Some(t), _) => mime::truncate_chars(t, 20_000),
            (None, Some(h)) => mime::truncate_chars(&mime::preview(None, Some(h)), 20_000),
            _ => String::new(),
        };
        self.conn.execute("UPDATE fts SET body = ?2 WHERE rowid = ?1", params![id, body_text])?;
        Ok(())
    }

    pub fn set_flags_by_uid(&self, account: &str, mailbox: &str, uid: u32, fl: u32) -> Result<()> {
        self.conn.execute("UPDATE messages SET flags = ?4 WHERE account = ?1 AND mailbox = ?2 AND uid = ?3", params![account, mailbox, uid, fl])?;
        Ok(())
    }

    pub fn delete_uids(&self, account: &str, mailbox: &str, uids: &[u32]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM messages WHERE account = ?1 AND mailbox = ?2 AND uid = ?3")?;
            for uid in uids {
                stmt.execute(params![account, mailbox, uid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self)?;
        tx.commit()?;
        Ok(out)
    }

    /// UIDs to download in full: the newest `limit` in the mailbox that have no body yet and
    /// are smaller than `max_size` bytes.
    pub fn missing_bodies(&self, account: &str, mailbox: &str, limit: u32, max_size: u32) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid FROM (SELECT uid, has_body, size FROM messages WHERE account = ?1 AND mailbox = ?2 ORDER BY date DESC LIMIT ?3)
             WHERE has_body = 0 AND size <= ?4",
        )?;
        let rows = stmt.query_map(params![account, mailbox, limit, max_size], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn id_for_uid(&self, account: &str, mailbox: &str, uid: u32) -> Result<Option<MessageId>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM messages WHERE account = ?1 AND mailbox = ?2 AND uid = ?3", params![account, mailbox, uid], |r| r.get(0))
            .optional()?)
    }

    pub fn summary(&self, id: MessageId) -> Result<Option<Summary>> {
        let sql = format!("SELECT {SUMMARY_COLS} FROM messages m WHERE m.id = ?1");
        Ok(self.conn.query_row(&sql, params![id], summary_row).optional()?.map(|(s, _)| s))
    }

    pub fn has_body(&self, id: MessageId) -> bool {
        self.conn.query_row("SELECT 1 FROM bodies WHERE id = ?1", params![id], |_| Ok(())).optional().ok().flatten().is_some()
    }

    pub fn raw(&self, id: MessageId) -> Result<Option<Vec<u8>>> {
        Ok(self.conn.query_row("SELECT raw FROM bodies WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    pub fn body(&self, id: MessageId) -> Result<Option<Body>> {
        let row = self
            .conn
            .query_row(
                "SELECT b.text, b.html, b.markdown, b.cc, b.reply_to, b.refs, b.attachments, m.message_id FROM bodies b JOIN messages m ON m.id = b.id WHERE b.id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((text, html, mut markdown, cc, reply_to, refs, attachments, message_id)) = row else { return Ok(None) };
        if markdown.is_none()
            && let Some(h) = &html
        {
            let md = mime::html_to_markdown(h);
            self.conn.execute("UPDATE bodies SET markdown = ?2 WHERE id = ?1", params![id, md])?;
            markdown = Some(md);
        }
        let attachments: Vec<AttachmentRow> = from_json(&attachments);
        let doc = html.as_deref().map(|h| std::sync::Arc::new(crate::html::parse(h)));
        Ok(Some(Body {
            doc,
            id,
            message_id,
            references: refs,
            cc: from_json(&cc),
            reply_to: from_json(&reply_to),
            text,
            html,
            markdown,
            attachments: attachments.into_iter().map(|a| Attachment { index: a.index, filename: a.filename, mime: a.mime, size: a.size }).collect(),
        }))
    }

    /// Applies a flag change locally; returns the message's location for queuing the op.
    pub fn change_flags(&self, id: MessageId, add: u32, remove: u32) -> Result<Option<(String, String, u32)>> {
        self.conn.execute("UPDATE messages SET flags = (flags | ?2) & ~?3 WHERE id = ?1", params![id, add, remove])?;
        self.location(id)
    }

    pub fn location(&self, id: MessageId) -> Result<Option<(String, String, u32)>> {
        Ok(self
            .conn
            .query_row("SELECT account, mailbox, uid FROM messages WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    /// Removes a message locally (it moved away or was expunged); the server copy follows via `ops`.
    pub fn remove_local(&self, id: MessageId) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// The other messages of `id`'s conversation that live in the same account (for actions on
    /// a whole conversation), newest first.
    pub fn thread_ids(&self, id: MessageId, view_mailbox_only: bool) -> Result<Vec<MessageId>> {
        let Some(s) = self.summary(id)? else { return Ok(vec![]) };
        let sql = if view_mailbox_only {
            "SELECT id FROM messages WHERE account = ?1 AND thread = ?2 AND mailbox = ?3 ORDER BY date DESC"
        } else {
            "SELECT id FROM messages WHERE account = ?1 AND thread = ?2 AND ?3 = ?3 ORDER BY date DESC"
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![s.account, s.thread, s.mailbox], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The list for a view, newest first. With `conversations`, one row per conversation.
    pub fn list(&self, view: &View, query: &Query, conversations: bool, limit: u32, me: &HashSet<String>) -> Result<Vec<Summary>> {
        let mut conds: Vec<String> = vec!["m.flags & 16 = 0".into()];
        let mut args: Vec<SqlValue> = Vec::new();
        match view {
            View::Mailbox { account, mailbox } => {
                conds.push("m.account = ? AND m.mailbox = ?".into());
                args.push(account.clone().into());
                args.push(mailbox.clone().into());
            }
            View::Role { account, role } => {
                conds.push("(m.account, m.mailbox) IN (SELECT account, name FROM mailboxes WHERE role = ?)".into());
                args.push(role.as_str().to_owned().into());
                if let Some(a) = account {
                    conds.push("m.account = ?".into());
                    args.push(a.clone().into());
                }
            }
            View::Starred { account } => {
                conds.push("m.flags & 2 != 0".into());
                conds.push("(m.account, m.mailbox) NOT IN (SELECT account, name FROM mailboxes WHERE role IN ('trash', 'junk'))".into());
                if let Some(a) = account {
                    conds.push("m.account = ?".into());
                    args.push(a.clone().into());
                }
            }
        }
        if !query.text.is_empty() {
            let m: Vec<String> = query.text.iter().map(|t| fts_term(t)).collect();
            conds.push("m.id IN (SELECT rowid FROM fts WHERE fts MATCH ?)".into());
            args.push(m.join(" AND ").into());
        }
        for f in &query.from {
            conds.push("(lower(m.from_email) LIKE ? OR lower(m.from_name) LIKE ?)".into());
            args.push(format!("%{f}%").into());
            args.push(format!("%{f}%").into());
        }
        for t in &query.to {
            conds.push("lower(m.recipients) LIKE ?".into());
            args.push(format!("%{t}%").into());
        }
        for t in &query.subject {
            conds.push("lower(m.subject) LIKE ?".into());
            args.push(format!("%{t}%").into());
        }
        match query.unread {
            Some(true) => conds.push("m.flags & 1 = 0".into()),
            Some(false) => conds.push("m.flags & 1 != 0".into()),
            None => {}
        }
        if query.starred == Some(true) {
            conds.push("m.flags & 2 != 0".into());
        }
        if query.attachment {
            conds.push("m.has_attachments = 1".into());
        }
        let fetch = if conversations { limit.saturating_mul(4).max(2000) } else { limit };
        let sql = format!("SELECT {SUMMARY_COLS} FROM messages m WHERE {} ORDER BY m.date DESC LIMIT {fetch}", conds.join(" AND "));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), summary_row)?;

        let mut out: Vec<Summary> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut thread_index: HashMap<(String, String), usize> = HashMap::new();
        let mut names: Vec<Vec<String>> = Vec::new();
        for row in rows {
            let (s, message_id) = row?;
            // The same message in two synced mailboxes (Gmail labels) shows once.
            if !message_id.is_empty() && !seen_ids.insert(format!("{}{}", s.account, message_id)) {
                continue;
            }
            if conversations {
                let key = (s.account.clone(), s.thread.clone());
                if let Some(&i) = thread_index.get(&key) {
                    let row = &mut out[i];
                    row.thread_count += 1;
                    row.thread_unread |= !s.seen();
                    row.has_attachments |= s.has_attachments;
                    if s.flagged() {
                        row.flags |= flags::FLAGGED;
                    }
                    let n = first_name(&s.from, me);
                    if !names[i].contains(&n) {
                        names[i].push(n);
                    }
                    continue;
                }
                if out.len() as u32 >= limit {
                    continue;
                }
                thread_index.insert(key, out.len());
                names.push(vec![first_name(&s.from, me)]);
            } else if out.len() as u32 >= limit {
                break;
            } else {
                names.push(vec![]);
            }
            let mut s = s;
            s.thread_unread = !s.seen();
            out.push(s);
        }
        for (s, n) in out.iter_mut().zip(names) {
            if s.thread_count > 1 {
                let mut n = n;
                n.reverse(); // oldest participant first, as Gmail shows them
                s.participants = n.join(", ");
            }
        }
        Ok(out)
    }

    /// An opened message, or its whole conversation (oldest first) with `conversations`.
    pub fn thread(&self, id: MessageId, conversations: bool) -> Result<Vec<ThreadMessage>> {
        let Some(s) = self.summary(id)? else { return Ok(vec![]) };
        let mut items = Vec::new();
        if conversations {
            let sql = format!(
                "SELECT {SUMMARY_COLS} FROM messages m WHERE m.account = ?1 AND m.thread = ?2 AND m.flags & 16 = 0
                 AND ((m.account, m.mailbox) NOT IN (SELECT account, name FROM mailboxes WHERE role IN ('trash', 'junk')) OR m.mailbox = ?3)
                 ORDER BY m.date ASC"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![s.account, s.thread, s.mailbox], summary_row)?;
            let mut seen = HashSet::new();
            for row in rows {
                let (m, mid) = row?;
                if !mid.is_empty() && !seen.insert(mid) {
                    continue;
                }
                items.push(m);
            }
        }
        if items.is_empty() {
            items.push(s);
        }
        items.into_iter().map(|summary| Ok(ThreadMessage { body: self.body(summary.id)?, summary })).collect()
    }

    // ── ops ──

    pub fn enqueue(&self, account: &str, op: &Op) -> Result<i64> {
        self.conn.execute("INSERT INTO ops (account, op) VALUES (?1, ?2)", params![account, json(op)])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn enqueue_at(&self, account: &str, op: &Op, not_before: i64) -> Result<i64> {
        self.conn.execute("INSERT INTO ops (account, op, not_before) VALUES (?1, ?2, ?3)", params![account, json(op), not_before])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn ops(&self, account: &str, now: i64) -> Result<Vec<QueuedOp>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, op, attempts FROM ops WHERE account = ?1 AND not_before <= ?2 ORDER BY id")?;
        let rows = stmt.query_map(params![account, now], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, op, attempts) = row?;
            match serde_json::from_str(&op) {
                Ok(op) => out.push(QueuedOp { id, op, attempts }),
                Err(_) => {
                    self.conn.execute("DELETE FROM ops WHERE id = ?1", params![id])?;
                }
            }
        }
        Ok(out)
    }

    /// Seconds until the next delayed op is due, if any.
    pub fn next_op_due(&self, account: &str, now: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MIN(not_before) FROM ops WHERE account = ?1 AND not_before > ?2", params![account, now], |r| r.get::<_, Option<i64>>(0))?
            .map(|t| t - now))
    }

    pub fn op_done(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM ops WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Cancels a queued op if it hasn't run (undo send). True when it was still queued.
    pub fn cancel_op(&self, id: i64) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM ops WHERE id = ?1", params![id])? > 0)
    }

    pub fn op_failed(&self, id: i64, error: &str, retry_in: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE ops SET attempts = attempts + 1, last_error = ?2, not_before = ?3 WHERE id = ?1",
            params![id, error, now + retry_in],
        )?;
        Ok(())
    }

    pub fn pending_count(&self) -> Result<u32> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))?)
    }

    pub fn forget_account(&self, account: &str) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE account = ?1", params![account])?;
        self.conn.execute("DELETE FROM mailboxes WHERE account = ?1", params![account])?;
        self.conn.execute("DELETE FROM ops WHERE account = ?1", params![account])?;
        Ok(())
    }

    /// Recomputes a mailbox's unread count from the local copy after a local change, so the
    /// sidebar moves at once (the next sync replaces it with the server's number).
    pub fn bump_unread(&self, account: &str, mailbox: &str, delta: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE mailboxes SET unread = MAX(0, unread + ?3) WHERE account = ?1 AND name = ?2",
            params![account, mailbox, delta],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(subject: &str, mid: &str, refs: &str, from: &str, hour: u32) -> Parsed {
        let raw = format!("From: {from}\r\nSubject: {subject}\r\nMessage-ID: {mid}\r\nReferences: {refs}\r\nDate: Tue, 3 Mar 2026 {hour:02}:00:00 +0000\r\n\r\nbody text about pancakes\r\n");
        mime::parse(raw.as_bytes(), false).unwrap()
    }

    #[test]
    fn conversations_group_and_search_finds() {
        let s = Store::memory();
        s.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        let p1 = parsed("Plans", "<1@x>", "", "Ana <ana@x.com>", 1);
        let p2 = parsed("Re: Plans", "<2@x>", "<1@x>", "Bo <bo@x.com>", 2);
        let p3 = parsed("Other", "<3@x>", "", "Cy <cy@x.com>", 3);
        let id1 = s.upsert_header("a", "INBOX", 1, 0, 10, 100, &p1).unwrap();
        s.upsert_header("a", "INBOX", 2, flags::SEEN, 10, 200, &p2).unwrap();
        s.upsert_header("a", "INBOX", 3, 0, 10, 300, &p3).unwrap();
        s.set_body(id1, b"raw", &p1).unwrap();
        let view = View::Role { account: None, role: Role::Inbox };
        let me = HashSet::new();
        let flat = s.list(&view, &Query::default(), false, 100, &me).unwrap();
        assert_eq!(flat.len(), 3);
        let conv = s.list(&view, &Query::default(), true, 100, &me).unwrap();
        assert_eq!(conv.len(), 2);
        let plans = conv.iter().find(|x| x.thread == "<1@x>").unwrap();
        assert_eq!(plans.thread_count, 2);
        assert!(plans.thread_unread);
        assert_eq!(plans.participants, "Ana, Bo");
        let hits = s.list(&view, &Query::parse("pancake"), false, 100, &me).unwrap();
        assert_eq!(hits.len(), 1);
        let hits = s.list(&view, &Query::parse("from:cy"), false, 100, &me).unwrap();
        assert_eq!(hits.len(), 1);
        let thread = s.thread(id1, true).unwrap();
        assert_eq!(thread.len(), 2);
        assert!(thread[0].body.is_some());
    }

    #[test]
    fn ops_queue_round_trips() {
        let s = Store::memory();
        let op = Op::SetFlags { mailbox: "INBOX".into(), uid: 4, add: flags::SEEN, remove: 0 };
        let id = s.enqueue("a", &op).unwrap();
        let ops = s.ops("a", 0).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, op);
        s.op_failed(id, "offline", 30, 0).unwrap();
        assert!(s.ops("a", 0).unwrap().is_empty());
        assert_eq!(s.ops("a", 31).unwrap().len(), 1);
    }
}
