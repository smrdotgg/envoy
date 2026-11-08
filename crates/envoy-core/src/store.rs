//! The local mail cache: SQLite in WAL mode, with FTS5 for instant search.
//!
//! The UI never waits on the network: it reads lists and bodies from here, and every action
//! (read, star, archive, delete, send) is applied here first and queued in `ops` for the sync
//! worker to replay on the server, retrying until it succeeds.
//!
//! Undoable actions (sending, moves) wait in `ops` until `not_before`. A message being moved
//! is hidden (`held_op`) rather than deleted until its move replays, so undo can show it again
//! untouched and a sync meanwhile doesn't download it a second time. The worker claims ops
//! before running them on the server; undo and cancel only touch unclaimed ones. Op ids are
//! never reused, so a stale id can't reach a newer op.

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
    id INTEGER PRIMARY KEY AUTOINCREMENT,
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
    held_op INTEGER NOT NULL DEFAULT 0,
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
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account TEXT NOT NULL,
    op TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    not_before INTEGER NOT NULL DEFAULT 0,
    batch INTEGER NOT NULL DEFAULT 0,
    claimed INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM fts WHERE rowid = old.id;
END;
"#;

/// Columns added after the first release, for caches created before them. Each runs once,
/// when the column is missing.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    // A message moved away locally whose move hasn't reached the server yet: hidden, not
    // deleted, so undo can bring it back and a sync can't re-download it meanwhile.
    ("messages", "held_op", "ALTER TABLE messages ADD COLUMN held_op INTEGER NOT NULL DEFAULT 0"),
    // Ops queued by one user action, so undo can take them all back.
    ("ops", "batch", "ALTER TABLE ops ADD COLUMN batch INTEGER NOT NULL DEFAULT 0"),
    // Taken by the worker for replay (on the wire): undo and cancel leave it alone.
    ("ops", "claimed", "ALTER TABLE ops ADD COLUMN claimed INTEGER NOT NULL DEFAULT 0"),
];

/// Indexes on migrated columns (they must exist first).
const POST_MIGRATION: &str = "
CREATE INDEX IF NOT EXISTS messages_held ON messages (held_op) WHERE held_op != 0;
CREATE INDEX IF NOT EXISTS messages_held_box ON messages (account, mailbox, flags, held_op) WHERE held_op != 0;
CREATE INDEX IF NOT EXISTS ops_batch ON ops (batch) WHERE batch != 0;
";

/// Op ids must never be reused: a toast's Undo or a replay in flight holds one, and a reused
/// id would let it cancel or complete a newer op. Caches from before AUTOINCREMENT get the
/// table rebuilt (same rows, same ids).
const OPS_AUTOINCREMENT: &str = "
BEGIN;
CREATE TABLE ops_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account TEXT NOT NULL,
    op TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    not_before INTEGER NOT NULL DEFAULT 0,
    batch INTEGER NOT NULL DEFAULT 0,
    claimed INTEGER NOT NULL DEFAULT 0
);
INSERT INTO ops_new (id, account, op, attempts, last_error, not_before, batch, claimed)
    SELECT id, account, op, attempts, last_error, not_before, batch, claimed FROM ops;
DROP TABLE ops;
ALTER TABLE ops_new RENAME TO ops;
COMMIT;
";

// Undo bookkeeping. Every successful op runs DELETE_HELD, so these must hit the partial
// indexes above, not scan the cache. SQLite only uses a partial index when the query
// itself implies the index's WHERE: hence the redundant `!= 0` terms.
const DELETE_HELD: &str = "DELETE FROM messages WHERE held_op = ?1 AND held_op != 0";
const HELD_ROWS: &str = "SELECT account, mailbox, flags FROM messages WHERE held_op = ?1 AND held_op != 0";
const UNHIDE: &str = "UPDATE messages SET held_op = 0 WHERE held_op = ?1 AND held_op != 0";
const BATCH_OPS: &str = "SELECT id, claimed FROM ops WHERE batch = ?1 AND batch != 0";

/// Message ids must never be reused either: an undo toast keeps a moved message's id to put
/// it back and reopen it, and a recycled id would reopen (and mark read) someone else's mail.
/// Caches from before AUTOINCREMENT get the table rebuilt with their ids kept, so bodies
/// (keyed by id) and search (rowid = id) stay attached. Indexes and the trigger go with the
/// old table and are recreated in the same transaction. Run with foreign keys off: with them
/// on, dropping the old table would cascade-delete every body.
const MESSAGES_AUTOINCREMENT: &str = "
BEGIN;
CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
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
    held_op INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account, mailbox, uid)
);
INSERT INTO messages_new (id, account, mailbox, uid, message_id, thread, subject, from_name, from_email, recipients, date, flags, size, preview, has_attachments, has_body, held_op)
    SELECT id, account, mailbox, uid, message_id, thread, subject, from_name, from_email, recipients, date, flags, size, preview, has_attachments, has_body, held_op FROM messages;
DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;
CREATE INDEX messages_box_date ON messages (account, mailbox, date DESC);
CREATE INDEX messages_thread ON messages (account, thread);
CREATE INDEX messages_flagged ON messages (flags) WHERE flags & 2;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM fts WHERE rowid = old.id;
END;
COMMIT;
";

/// The server's counts minus messages waiting for a move (see [`Store::set_counts`]). Runs
/// under the writer lock after every STATUS, so the subqueries must not scan the mailbox:
/// they read only the few held rows, from messages_held_box.
const SET_COUNTS: &str = "UPDATE mailboxes SET
    unread = MAX(0, ?3 - (SELECT COUNT(*) FROM messages WHERE account = ?1 AND mailbox = ?2 AND held_op != 0 AND flags & 1 = 0)),
    total = MAX(0, ?4 - (SELECT COUNT(*) FROM messages WHERE account = ?1 AND mailbox = ?2 AND held_op != 0))
 WHERE account = ?1 AND name = ?2";

fn has_autoincrement(conn: &Connection, table: &str) -> Result<bool> {
    let sql: String = conn.query_row("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1", params![table], |r| r.get(0))?;
    Ok(sql.contains("AUTOINCREMENT"))
}

fn migrate(conn: &Connection) -> Result<()> {
    for (table, column, sql) in MIGRATIONS {
        let has: bool = conn.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"), params![column], |r| r.get::<_, i64>(0))? > 0;
        if !has {
            conn.execute_batch(sql)?;
        }
    }
    if !has_autoincrement(conn, "ops")? {
        conn.execute_batch(OPS_AUTOINCREMENT)?;
    }
    if !has_autoincrement(conn, "messages")? {
        // PRAGMA foreign_keys is ignored inside a transaction, hence outside the batch.
        conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        let rebuilt = conn.execute_batch(MESSAGES_AUTOINCREMENT);
        if rebuilt.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        rebuilt?;
    }
    conn.execute_batch(POST_MIGRATION)?;
    Ok(())
}

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

/// What [`Store::undo_batch`] managed.
#[derive(Debug, Default, PartialEq)]
pub struct Undone {
    /// Accounts whose messages came back.
    pub accounts: Vec<String>,
    /// Ops cancelled before reaching the server.
    pub released: usize,
    /// Ops already on their way to the server, left to finish.
    pub in_flight: usize,
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
        migrate(&conn)?;
        Ok(Store { conn })
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
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

    /// Applies the server's counts. Messages waiting for a move to replay are still on the
    /// server, so they're in its counts, but they've already left this mailbox locally (and
    /// undo adds them back): they're taken out here so nothing is counted twice.
    pub fn set_counts(&self, account: &str, name: &str, unread: u32, total: u32) -> Result<()> {
        self.conn.execute(SET_COUNTS, params![account, name, unread, total])?;
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

    /// Applies what the server reported about the messages we already had (`local`, read
    /// before fetching; `server`: uid → flags): new flags, deletions, then trims the mailbox to
    /// its newest `window` messages. True when anything changed.
    pub fn reconcile(&self, account: &str, mailbox: &str, local: &[(u32, u32)], server: &HashMap<u32, u32>, window: u32) -> Result<bool> {
        let mut changed = false;
        let mut gone = Vec::new();
        self.transaction(|st| {
            for (uid, local_flags) in local {
                match server.get(uid) {
                    None => gone.push(*uid),
                    Some(&f) if f != *local_flags => {
                        st.set_flags_by_uid(account, mailbox, *uid, f)?;
                        changed = true;
                    }
                    _ => {}
                }
            }
            Ok(())
        })?;
        if !gone.is_empty() {
            self.delete_uids(account, mailbox, &gone)?;
            changed = true;
        }
        // Trim: keep the newest `window` visible messages. Messages waiting for a move to
        // replay don't count and are never evicted: undo needs them, and a sync (which only
        // fetches UIDs above the highest one cached) would never bring them back.
        let trimmed = self.conn.execute(
            "DELETE FROM messages WHERE id IN (SELECT id FROM messages WHERE account = ?1 AND mailbox = ?2 AND held_op = 0 ORDER BY uid DESC LIMIT -1 OFFSET ?3)",
            params![account, mailbox, window],
        )?;
        Ok(changed || trimmed > 0)
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
            "SELECT uid FROM (SELECT uid, has_body, size FROM messages WHERE account = ?1 AND mailbox = ?2 AND held_op = 0 ORDER BY date DESC LIMIT ?3)
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

    /// Any cached message, hidden or not (for bookkeeping; what the user sees goes through
    /// [`Store::list`] and [`Store::thread`]).
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

    /// Removes a message locally (it was expunged); the server copy follows via `ops`.
    pub fn remove_local(&self, id: MessageId) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Where a message is, if it can be moved: not when it's already hidden for a move (that
    /// move may be on the wire; a second one would fight over it).
    pub fn movable(&self, id: MessageId) -> Result<Option<(String, String, u32)>> {
        Ok(self
            .conn
            .query_row("SELECT account, mailbox, uid FROM messages WHERE id = ?1 AND held_op = 0", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    /// Hides a message that is moving away until `op` (the move) reaches the server, when it is
    /// deleted ([`Store::op_done`]), or is undone or dropped, when it shows again. Its mailbox's
    /// unread and total counts drop at once ([`Store::release`] puts them back). Never takes
    /// over an existing hold: false (and nothing changes) if the message is already moving.
    /// Call inside a transaction.
    pub fn hide_local(&self, id: MessageId, op: i64) -> Result<bool> {
        if self.conn.execute("UPDATE messages SET held_op = ?2 WHERE id = ?1 AND held_op = 0", params![id, op])? == 0 {
            return Ok(false);
        }
        let (account, mailbox, fl): (String, String, u32) =
            self.conn.query_row("SELECT account, mailbox, flags FROM messages WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        self.bump_counts(&account, &mailbox, if fl & flags::SEEN == 0 { -1 } else { 0 }, -1)?;
        Ok(true)
    }

    /// The other messages of `id`'s conversation that live in the same account (for actions on
    /// a whole conversation), newest first.
    pub fn thread_ids(&self, id: MessageId, view_mailbox_only: bool) -> Result<Vec<MessageId>> {
        let Some(s) = self.summary(id)? else { return Ok(vec![]) };
        let sql = if view_mailbox_only {
            "SELECT id FROM messages WHERE account = ?1 AND thread = ?2 AND mailbox = ?3 AND held_op = 0 ORDER BY date DESC"
        } else {
            "SELECT id FROM messages WHERE account = ?1 AND thread = ?2 AND ?3 = ?3 AND held_op = 0 ORDER BY date DESC"
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![s.account, s.thread, s.mailbox], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The list for a view, newest first. With `conversations`, one row per conversation.
    pub fn list(&self, view: &View, query: &Query, conversations: bool, limit: u32, me: &HashSet<String>) -> Result<Vec<Summary>> {
        let mut conds: Vec<String> = vec!["m.flags & 16 = 0".into(), "m.held_op = 0".into()];
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
    /// Empty for a message that's gone, or hidden while its move waits to replay.
    pub fn thread(&self, id: MessageId, conversations: bool) -> Result<Vec<ThreadMessage>> {
        let sql = format!("SELECT {SUMMARY_COLS} FROM messages m WHERE m.id = ?1 AND m.held_op = 0");
        let Some((s, _)) = self.conn.query_row(&sql, params![id], summary_row).optional()? else { return Ok(vec![]) };
        let mut items = Vec::new();
        if conversations {
            let sql = format!(
                "SELECT {SUMMARY_COLS} FROM messages m WHERE m.account = ?1 AND m.thread = ?2 AND m.flags & 16 = 0 AND m.held_op = 0
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

    /// Due ops not taken for replay, oldest first. Read-only apart from dropping unreadable
    /// ones; replay uses [`Store::claim_due`].
    pub fn ops(&self, account: &str, now: i64) -> Result<Vec<QueuedOp>> {
        self.transaction(|st| st.due(account, now))
    }

    /// Takes the account's due ops for replay, atomically: from here until [`Store::op_done`],
    /// [`Store::op_failed`] or [`Store::unclaim`], undo and cancel can't touch them, because
    /// the server may already be carrying them out. Claims left over from a replay that never
    /// finished (a crash) are released first. Only the account's worker calls this, one pass
    /// at a time.
    pub fn claim_due(&self, account: &str, now: i64) -> Result<Vec<QueuedOp>> {
        self.transaction(|st| {
            st.unclaim(account)?;
            let due = st.due(account, now)?;
            let mut claim = st.conn.prepare_cached("UPDATE ops SET claimed = 1 WHERE id = ?1")?;
            for q in &due {
                claim.execute(params![q.id])?;
            }
            Ok(due)
        })
    }

    /// Hands back the account's claimed ops that didn't finish (replay stopped early).
    pub fn unclaim(&self, account: &str) -> Result<()> {
        self.conn.execute("UPDATE ops SET claimed = 0 WHERE account = ?1 AND claimed = 1", params![account])?;
        Ok(())
    }

    /// Call inside a transaction.
    fn due(&self, account: &str, now: i64) -> Result<Vec<QueuedOp>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, op, attempts FROM ops WHERE account = ?1 AND not_before <= ?2 AND claimed = 0 ORDER BY id")?;
        let rows = stmt.query_map(params![account, now], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?)))?;
        let mut out = Vec::new();
        let mut unreadable = Vec::new();
        for row in rows {
            let (id, op, attempts) = row?;
            match serde_json::from_str(&op) {
                Ok(op) => out.push(QueuedOp { id, op, attempts }),
                Err(_) => unreadable.push(id),
            }
        }
        drop(stmt);
        if !unreadable.is_empty() {
            self.release(&unreadable)?;
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

    /// An op that can be undone: it waits until `not_before` and is tagged with the user
    /// action (`batch`) it belongs to, so [`Store::undo_batch`] can take the whole action back.
    pub fn enqueue_held(&self, account: &str, op: &Op, not_before: i64, batch: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO ops (account, op, not_before, batch) VALUES (?1, ?2, ?3, ?4)",
            params![account, json(op), not_before, batch],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The op reached the server. A message it was moving away now leaves the local copy too.
    pub fn op_done(&self, id: i64) -> Result<()> {
        self.transaction(|st| {
            st.conn.execute(DELETE_HELD, params![id])?;
            st.conn.execute("DELETE FROM ops WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Gives up on an op the server keeps refusing. A message it was moving away shows again
    /// where it still is. Returns the op's account (None if it was already gone).
    pub fn op_dropped(&self, id: i64) -> Result<Option<String>> {
        self.transaction(|st| Ok(st.release(&[id])?.into_iter().next()))
    }

    /// Cancels a queued send if it hasn't run (undo send). True when it was still queued.
    /// Refuses any other kind of op, so a confused caller can't strand a moved message.
    pub fn cancel_send(&self, id: i64) -> Result<bool> {
        let op: Option<String> = self.conn.query_row("SELECT op FROM ops WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        if !op.is_some_and(|o| matches!(serde_json::from_str(&o), Ok(Op::Send { .. }))) {
            return Ok(false);
        }
        Ok(self.conn.execute("DELETE FROM ops WHERE id = ?1 AND claimed = 0", params![id])? > 0)
    }

    /// Undoes a user action: cancels its ops that haven't gone to the server and shows the
    /// messages they were moving again, unread counts included. Ops already claimed for replay
    /// are left to finish (the server may be carrying them out): they're counted in
    /// `in_flight`. Ops that already replayed are simply gone.
    pub fn undo_batch(&self, batch: i64) -> Result<Undone> {
        if batch == 0 {
            return Ok(Undone::default());
        }
        self.transaction(|st| {
            let ops: Vec<(i64, bool)> = st
                .conn
                .prepare_cached(BATCH_OPS)?
                .query_map(params![batch], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)))?
                .collect::<rusqlite::Result<_>>()?;
            let unclaimed: Vec<i64> = ops.iter().filter(|(_, c)| !c).map(|(id, _)| *id).collect();
            Ok(Undone { accounts: st.release(&unclaimed)?, released: unclaimed.len(), in_flight: ops.len() - unclaimed.len() })
        })
    }

    /// Deletes these ops and un-hides what they were moving. Call inside a transaction.
    fn release(&self, ops: &[i64]) -> Result<Vec<String>> {
        let mut accounts: Vec<String> = Vec::new();
        for op in ops {
            let account: Option<String> = self.conn.query_row("SELECT account FROM ops WHERE id = ?1", params![op], |r| r.get(0)).optional()?;
            // Back in their mailbox: undo what hide_local did to its counts.
            let held: Vec<(String, String, u32)> = self
                .conn
                .prepare_cached(HELD_ROWS)?
                .query_map(params![op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (a, m, fl) in held {
                self.bump_counts(&a, &m, if fl & flags::SEEN == 0 { 1 } else { 0 }, 1)?;
            }
            self.conn.execute(UNHIDE, params![op])?;
            self.conn.execute("DELETE FROM ops WHERE id = ?1", params![op])?;
            if let Some(a) = account
                && !accounts.contains(&a)
            {
                accounts.push(a);
            }
        }
        Ok(accounts)
    }

    pub fn op_failed(&self, id: i64, error: &str, retry_in: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE ops SET attempts = attempts + 1, last_error = ?2, not_before = ?3, claimed = 0 WHERE id = ?1",
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
        self.bump_counts(account, mailbox, delta, 0)
    }

    /// [`Store::bump_unread`], for the total too (a message left or came back).
    pub fn bump_counts(&self, account: &str, mailbox: &str, unread: i64, total: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE mailboxes SET unread = MAX(0, unread + ?3), total = MAX(0, total + ?4) WHERE account = ?1 AND name = ?2",
            params![account, mailbox, unread, total],
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

    /// One unread, starred inbox message with its body downloaded, moved away under undo
    /// key 7, held until t=100.
    fn held_move(s: &Store) -> (MessageId, i64) {
        s.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        s.upsert_mailbox("a", "Archive", "Archive", Role::Archive, "/", true).unwrap();
        s.set_counts("a", "INBOX", 1, 1).unwrap();
        let p = parsed("Hi", "<9@x>", "", "Ana <ana@x.com>", 1);
        let id = s.upsert_header("a", "INBOX", 9, flags::FLAGGED, 10, 100, &p).unwrap();
        s.set_body(id, b"raw", &p).unwrap();
        let op = s.enqueue_held("a", &Op::Move { mailbox: "INBOX".into(), uid: 9, dest: "Archive".into() }, 100, 7).unwrap();
        assert!(s.hide_local(id, op).unwrap());
        (id, op)
    }

    fn inbox(s: &Store) -> Vec<Summary> {
        s.list(&View::Role { account: None, role: Role::Inbox }, &Query::default(), true, 100, &HashSet::new()).unwrap()
    }

    fn unread(s: &Store) -> u32 {
        s.mailboxes("a").unwrap().iter().find(|m| m.name == "INBOX").unwrap().unread
    }

    /// Messages hidden for an op that no longer exists: they would stay hidden forever.
    fn orphaned_holds(s: &Store) -> i64 {
        s.conn.query_row("SELECT COUNT(*) FROM messages WHERE held_op != 0 AND held_op NOT IN (SELECT id FROM ops)", [], |r| r.get(0)).unwrap()
    }

    fn send_op() -> Op {
        Op::Send { raw_b64: String::new(), from: "me@x.com".into(), to: vec!["you@x.com".into()], sent_mailbox: None, reply_of: None }
    }

    #[test]
    fn a_stale_undo_send_cant_cancel_a_later_move() {
        let s = Store::memory();
        let sent = s.enqueue_at("a", &send_op(), 50).unwrap();
        assert!(s.cancel_send(sent).unwrap(), "undo send");
        let (_, moved) = held_move(&s);
        assert_ne!(moved, sent, "op ids are never reused");
        // The old "Sending…" toast's Undo once more (say, a double click that raced it).
        assert!(!s.cancel_send(sent).unwrap());
        assert!(!s.cancel_send(moved).unwrap(), "only sends are cancelled this way");
        assert_eq!(s.ops("a", 100).unwrap().len(), 1, "the move is still queued");
        assert_eq!(orphaned_holds(&s), 0);
    }

    #[test]
    fn op_ids_stay_unique_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("envoy-store-ids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        let first = {
            let s = Store::open(&db).unwrap();
            let id = s.enqueue("a", &send_op()).unwrap();
            assert!(s.cancel_send(id).unwrap());
            id
        };
        let s = Store::open(&db).unwrap();
        assert!(s.enqueue("a", &send_op()).unwrap() > first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_move_op_lets_go_of_its_message() {
        let s = Store::memory();
        let (id, op) = held_move(&s);
        s.conn.execute("UPDATE ops SET op = 'not json' WHERE id = ?1", params![op]).unwrap();
        assert!(s.ops("a", 100).unwrap().is_empty(), "dropped");
        assert_eq!(orphaned_holds(&s), 0);
        assert_eq!(inbox(&s)[0].id, id, "shows again");
    }

    #[test]
    fn claimed_ops_are_out_of_reach_of_undo_and_cancel() {
        let s = Store::memory();
        let (id, op) = held_move(&s);
        let send = s.enqueue_at("a", &send_op(), 0).unwrap();
        let claimed: Vec<i64> = s.claim_due("a", 100).unwrap().iter().map(|q| q.id).collect();
        assert_eq!(claimed, vec![op, send]);
        assert!(s.ops("a", 100).unwrap().is_empty(), "claimed ops aren't due for anyone else");
        assert!(!s.cancel_send(send).unwrap(), "a send on the wire can't be undone");
        assert_eq!(s.undo_batch(7).unwrap(), Undone { accounts: vec![], released: 0, in_flight: 1 });
        assert!(inbox(&s).is_empty(), "the move may be happening: stays hidden");
        s.op_done(op).unwrap();
        assert!(s.summary(id).unwrap().is_none());
        // A failure hands the op back to the queue, where undo can reach it again.
        s.op_failed(send, "offline", 0, 100).unwrap();
        assert!(s.cancel_send(send).unwrap());
        assert_eq!(orphaned_holds(&s), 0);
    }

    #[test]
    fn claims_left_by_a_crash_are_retried() {
        let dir = std::env::temp_dir().join(format!("envoy-store-claims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        let op = {
            let s = Store::open(&db).unwrap();
            let (_, op) = held_move(&s);
            assert_eq!(s.claim_due("a", 100).unwrap().len(), 1);
            op // the app dies here, mid-replay
        };
        let s = Store::open(&db).unwrap();
        let again: Vec<i64> = s.claim_due("a", 100).unwrap().iter().map(|q| q.id).collect();
        assert_eq!(again, vec![op], "replayed on the next start");
        s.unclaim("a").unwrap();
        assert_eq!(s.ops("a", 100).unwrap().len(), 1, "unclaim puts it back in the queue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trimming_the_cache_keeps_messages_waiting_to_move() {
        let s = Store::memory();
        s.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        let ids: Vec<MessageId> =
            (1..=3).map(|uid| s.upsert_header("a", "INBOX", uid, 0, 10, 0, &parsed("Hi", &format!("<{uid}@x>"), "", "Ana <ana@x.com>", uid)).unwrap()).collect();
        // The cache is full (window 3); the oldest message is archived, undoably.
        let local = s.local_uids("a", "INBOX").unwrap();
        let op = s.enqueue_held("a", &Op::Move { mailbox: "INBOX".into(), uid: 1, dest: "Archive".into() }, 100, 7).unwrap();
        assert!(s.hide_local(ids[0], op).unwrap());
        // New mail arrives before the window ends, and the sync trims to the window.
        s.upsert_header("a", "INBOX", 4, 0, 10, 0, &parsed("New", "<4@x>", "", "Bo <bo@x.com>", 4)).unwrap();
        let server: HashMap<u32, u32> = (1..=4).map(|uid| (uid, 0)).collect();
        assert!(!s.reconcile("a", "INBOX", &local, &server, 3).unwrap(), "three visible messages: nothing to trim");
        s.undo_batch(7).unwrap();
        assert!(inbox(&s).iter().any(|m| m.id == ids[0]), "undo still finds it");
        // Visible messages beyond the window still go.
        let local = s.local_uids("a", "INBOX").unwrap();
        assert!(s.reconcile("a", "INBOX", &local, &server, 3).unwrap());
        assert_eq!(s.local_uids("a", "INBOX").unwrap().iter().map(|(u, _)| *u).collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn undo_bookkeeping_uses_its_indexes() {
        let s = Store::memory();
        for (sql, index) in [(DELETE_HELD, "messages_held"), (HELD_ROWS, "messages_held"), (UNHIDE, "messages_held"), (BATCH_OPS, "ops_batch")] {
            let plan: Vec<String> =
                s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap().query_map(params![5], |r| r.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            let searches = plan.iter().any(|d| d.starts_with("SEARCH") && d.contains(&format!("INDEX {index} ")));
            assert!(searches && !plan.iter().any(|d| d.starts_with("SCAN")), "{sql}\n  plan: {plan:?}");
        }
    }

    #[test]
    fn server_counts_dont_count_held_messages_twice() {
        let s = Store::memory();
        held_move(&s);
        assert_eq!(unread(&s), 0);
        // A sync during the window: the server still has the message in INBOX.
        s.set_counts("a", "INBOX", 1, 1).unwrap();
        let inbox_box = || s.mailboxes("a").unwrap().into_iter().find(|m| m.name == "INBOX").unwrap();
        assert_eq!((inbox_box().unread, inbox_box().total), (0, 0), "the held message isn't counted");
        s.undo_batch(7).unwrap();
        assert_eq!(unread(&s), 1, "one unread message, one unread");
    }

    #[test]
    fn a_hidden_message_cant_be_opened() {
        let s = Store::memory();
        let (id, _) = held_move(&s);
        assert!(s.thread(id, false).unwrap().is_empty(), "flat");
        assert!(s.thread(id, true).unwrap().is_empty(), "conversation");
        assert!(s.summary(id).unwrap().is_some(), "move bookkeeping still sees it");
        s.undo_batch(7).unwrap();
        assert_eq!(s.thread(id, false).unwrap().len(), 1);
    }

    #[test]
    fn message_ids_are_never_reused() {
        let s = Store::memory();
        // The newest cached message is archived and its move replays.
        let (archived, op) = held_move(&s);
        s.op_done(op).unwrap();
        // New mail arrives: it must not take the archived message's id, which its undo toast
        // (and the reader) still hold.
        let new = s.upsert_header("a", "INBOX", 10, 0, 10, 0, &parsed("New", "<10@x>", "", "Bo <bo@x.com>", 2)).unwrap();
        assert!(new > archived);
        assert!(s.thread(archived, false).unwrap().is_empty(), "the old id opens nothing");
    }

    #[test]
    fn old_caches_keep_messages_bodies_and_search_through_the_id_migration() {
        let dir = std::env::temp_dir().join(format!("envoy-store-msgids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        let (old_id, other) = (5, 6);
        {
            // A cache from before ids were AUTOINCREMENT, with a downloaded body and search entry.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(&SCHEMA.replace(" AUTOINCREMENT", "")).unwrap();
            conn.execute_batch(
                "INSERT INTO mailboxes (account, name, display, role) VALUES ('a', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, date) VALUES
                     (5, 'a', 'INBOX', 1, '<1@x>', '<1@x>', 'Pancakes', 100),
                     (6, 'a', 'INBOX', 2, '<2@x>', '<2@x>', 'Waffles', 200);
                 INSERT INTO bodies (id, raw, text) VALUES (5, 'raw', 'all about pancakes');
                 INSERT INTO fts (rowid, subject, people, body) VALUES (5, 'Pancakes', 'Ana', 'all about pancakes'), (6, 'Waffles', 'Bo', '');",
            )
            .unwrap();
            let sql: String = conn.query_row("SELECT sql FROM sqlite_master WHERE name = 'messages'", [], |r| r.get(0)).unwrap();
            assert!(!sql.contains("AUTOINCREMENT"), "set up an old cache");
        }
        let s = Store::open(&db).unwrap();
        let opened = s.thread(old_id, false).unwrap();
        assert_eq!(opened[0].summary.subject, "Pancakes", "same ids");
        assert!(opened[0].body.is_some(), "bodies survive the rebuild");
        let hits = s.list(&View::Role { account: None, role: Role::Inbox }, &Query::parse("pancakes"), false, 10, &HashSet::new()).unwrap();
        assert_eq!(hits.iter().map(|m| m.id).collect::<Vec<_>>(), vec![old_id], "search still finds it");
        // The delete trigger is back: removing a message removes it from search.
        s.remove_local(old_id).unwrap();
        assert!(s.list(&View::Role { account: None, role: Role::Inbox }, &Query::parse("pancakes"), false, 10, &HashSet::new()).unwrap().is_empty());
        s.remove_local(other).unwrap();
        let next = s.upsert_header("a", "INBOX", 3, 0, 10, 0, &parsed("New", "<3@x>", "", "Cy <cy@x.com>", 3)).unwrap();
        assert!(next > other, "ids not reused after the migration");
        let indexes: i64 = s.conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages' AND name LIKE 'messages_%'", [], |r| r.get(0)).unwrap();
        assert!(indexes >= 4, "indexes rebuilt ({indexes})");
        let fk_on: i64 = s.conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk_on, 1, "foreign keys back on");
        let dangling: i64 = s.conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
        assert_eq!(dangling, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn counts(s: &Store) -> (u32, u32) {
        let m = s.mailboxes("a").unwrap().into_iter().find(|m| m.name == "INBOX").unwrap();
        (m.unread, m.total)
    }

    #[test]
    fn hiding_and_undoing_keep_unread_and_total_in_step() {
        let s = Store::memory();
        held_move(&s); // the inbox's only message, unread
        assert_eq!(counts(&s), (0, 0), "it left the inbox");
        s.undo_batch(7).unwrap();
        assert_eq!(counts(&s), (1, 1), "and came back");
    }

    #[test]
    fn a_hold_is_never_replaced() {
        let s = Store::memory();
        let (id, op) = held_move(&s);
        assert_eq!(s.movable(id).unwrap(), None, "a moving message can't be moved again");
        let second = s.enqueue_held("a", &Op::Move { mailbox: "INBOX".into(), uid: 9, dest: "Junk".into() }, 100, 8).unwrap();
        assert!(!s.hide_local(id, second).unwrap(), "refused");
        s.undo_batch(8).unwrap();
        s.op_done(op).unwrap();
        assert!(s.summary(id).unwrap().is_none(), "the first move still owns it");
    }

    #[test]
    fn server_counts_dont_scan_a_big_mailbox() {
        let s = Store::memory();
        s.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        s.transaction(|st| {
            for uid in 1..=5_000u32 {
                st.conn.execute(
                    "INSERT INTO messages (account, mailbox, uid, date, flags, held_op) VALUES ('a', 'INBOX', ?1, ?1, ?2, ?3)",
                    params![uid, uid % 2, if uid % 1000 == 0 { uid as i64 } else { 0 }],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let plan = || -> Vec<String> {
            s.conn
                .prepare(&format!("EXPLAIN QUERY PLAN {SET_COUNTS}"))
                .unwrap()
                .query_map(params!["a", "INBOX", 1, 1], |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for stats in ["without statistics", "after ANALYZE"] {
            if stats == "after ANALYZE" {
                s.conn.execute_batch("ANALYZE").unwrap();
            }
            let plan = plan();
            let on_messages: Vec<&String> = plan.iter().filter(|d| d.contains(" messages ")).collect();
            assert_eq!(on_messages.len(), 2, "{stats}: {plan:?}");
            assert!(on_messages.iter().all(|d| d.starts_with("SEARCH") && d.contains("COVERING INDEX messages_held_box ")), "{stats}: {plan:?}");
        }
        s.set_counts("a", "INBOX", 2_500, 5_000).unwrap();
        let m = s.mailboxes("a").unwrap().remove(0);
        assert_eq!((m.unread, m.total), (2_495, 4_995), "five held, all unread");
    }

    #[test]
    fn held_move_hides_but_keeps_the_row() {
        let s = Store::memory();
        let (id, _) = held_move(&s);
        assert!(inbox(&s).is_empty(), "gone from the list");
        assert!(s.thread_ids(id, true).unwrap().is_empty(), "not moved again with its conversation");
        assert_eq!(s.local_uids("a", "INBOX").unwrap(), vec![(9, flags::FLAGGED)], "sync still sees it, so it isn't re-downloaded");
        // A sync during the window refreshes flags without showing it again.
        s.upsert_header("a", "INBOX", 9, flags::SEEN, 10, 100, &parsed("Hi", "<9@x>", "", "Ana <ana@x.com>", 1)).unwrap();
        assert!(inbox(&s).is_empty());
    }

    #[test]
    fn held_op_waits_for_its_window() {
        let s = Store::memory();
        held_move(&s);
        assert!(s.ops("a", 99).unwrap().is_empty(), "not replayed during the window");
        assert_eq!(s.next_op_due("a", 95).unwrap(), Some(5), "the worker wakes when it ends");
        assert_eq!(s.ops("a", 100).unwrap().len(), 1, "replayed once it ends");
    }

    #[test]
    fn undo_cancels_and_restores() {
        let s = Store::memory();
        let (id, _) = held_move(&s);
        assert_eq!(unread(&s), 0);
        assert_eq!(s.undo_batch(7).unwrap().accounts, vec!["a".to_owned()]);
        assert!(s.ops("a", i64::MAX).unwrap().is_empty(), "the server never sees it");
        let back = inbox(&s);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, id, "the same row, so the app can reopen it by id");
        assert_eq!(back[0].flags, flags::FLAGGED, "flags intact: still unread and starred");
        assert!(back[0].preview.contains("pancakes"));
        let opened = s.thread(id, false).unwrap();
        let body = opened[0].body.as_ref().expect("the downloaded body is still there");
        assert_eq!(body.text.as_deref(), Some("body text about pancakes\r\n"), "no re-download needed");
        assert_eq!(unread(&s), 1, "unread count back");
        assert_eq!(s.undo_batch(7).unwrap(), Undone::default(), "nothing left to undo");
        assert_eq!(s.undo_batch(0).unwrap(), Undone::default(), "key 0 is never an undo key");
    }

    #[test]
    fn replayed_move_deletes_the_row_and_cant_be_undone() {
        let s = Store::memory();
        let (id, op) = held_move(&s);
        s.op_done(op).unwrap();
        assert!(s.summary(id).unwrap().is_none());
        assert_eq!(s.undo_batch(7).unwrap(), Undone::default());
        assert!(inbox(&s).is_empty());
    }

    #[test]
    fn dropped_move_shows_the_message_again() {
        let s = Store::memory();
        let (id, op) = held_move(&s);
        assert_eq!(s.op_dropped(op).unwrap(), Some("a".to_owned()));
        assert_eq!(inbox(&s)[0].id, id);
        assert_eq!(s.pending_count().unwrap(), 0);
    }

    #[test]
    fn held_moves_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("envoy-store-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        {
            let s = Store::open(&db).unwrap();
            held_move(&s);
        }
        let s = Store::open(&db).unwrap();
        assert!(inbox(&s).is_empty(), "still hidden after restart");
        let due = s.ops("a", 1_000).unwrap();
        assert_eq!(due.len(), 1, "a window that ran out while closed just replays");
        s.op_done(due[0].id).unwrap();
        assert_eq!(s.local_uids("a", "INBOX").unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_caches_gain_the_new_columns() {
        let dir = std::env::temp_dir().join(format!("envoy-store-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        {
            let old = SCHEMA
                .replace("    held_op INTEGER NOT NULL DEFAULT 0,\n", "")
                .replace(",\n    batch INTEGER NOT NULL DEFAULT 0", "")
                .replace(" AUTOINCREMENT", "");
            assert!(!old.contains("held_op") && !old.contains("batch") && !old.contains("AUTOINCREMENT"));
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(&old).unwrap();
            conn.execute("INSERT INTO ops (id, account, op) VALUES (41, 'a', ?1)", params![json(&send_op())]).unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.ops("a", 0).unwrap()[0].id, 41, "queued ops kept, same ids");
        let (_, op) = held_move(&s);
        assert!(op > 41);
        assert_eq!(s.undo_batch(7).unwrap().accounts, vec!["a".to_owned()]);
        drop(s);
        Store::open(&db).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
