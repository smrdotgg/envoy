//! The split inbox's side of the store: each conversation's category (`messages.category`, the
//! same on every message of it), worked out as messages are stored and again whenever what it
//! depends on changes. How a category is decided is `category.rs`'s business.
//!
//! - Storing a message ([`Store::upsert_header`]) reads what its headers say (`signals`,
//!   `list_id`) and marks its conversation. Mail you send adds its recipients to `written_to`,
//!   and their conversations are marked too: mail from someone you've written to is Important.
//! - Marked conversations are worked out once per write: at the end of [`Store::transaction`]
//!   (a chunk of a download is one), or of the write itself outside one. A conversation is read
//!   by the thread index, so it costs what the conversation has, however big the cache.
//! - A new classifier ([`Store::set_classifier`]: the rules or your addresses changed) that only
//!   adds or drops rules for single senders, as corrections do, moves those senders' mail at once
//!   (by `messages_from`). Anything else starts a pass over the whole cache ([`Store::recategorize`]),
//!   newest first, a chunk at a time, that the engine runs in the background: it survives a
//!   restart (`category_state`), and lists stay readable throughout (they show what's done).
//! - Messages cached by an Envoy from before the split have no `signals` (their headers weren't
//!   read): the pass reads them from the message itself where it was downloaded, and the rest
//!   are judged by what the cache has until their body arrives ([`Store::set_body`]).

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Store, from_json, json};
use crate::category::{self, Classifier, Facts, Verdict, signals};
use crate::mime::{self, Parsed};
use crate::model::{Address, Category, MessageId, Role, View, flags};

/// What one call of [`Store::recategorize`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pass {
    /// A pass is still under way.
    pub more: bool,
    /// Messages whose category changed.
    pub changed: usize,
}

/// A conversation moved to a category by hand ([`Store::move_to_category`]), and what it had
/// been moved to before (None: nothing), to take it back.
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    pub account: String,
    pub thread: String,
    pub before: Option<Category>,
}

/// Mailboxes' roles (`(account, mailbox, role)`), read once per batch: a few dozen.
#[derive(Default)]
struct Roles(Vec<(String, String, Role)>, Vec<String>);

impl Roles {
    fn role(&mut self, st: &Store, account: &str, mailbox: &str) -> Result<Role> {
        if !self.1.iter().any(|a| a == account) {
            let mut stmt = st.conn.prepare_cached("SELECT name, role FROM mailboxes WHERE account = ?1")?;
            let mut rows = stmt.query([account])?;
            while let Some(r) = rows.next()? {
                self.0.push((account.to_owned(), r.get(0)?, Role::parse(&r.get::<_, String>(1)?)));
            }
            self.1.push(account.to_owned());
        }
        Ok(self.0.iter().find(|(a, m, _)| a == account && m == mailbox).map_or(Role::Other, |r| r.2))
    }
}

/// A conversation's key, as list.rs keys them.
fn key(account: &str, thread: &str) -> String {
    format!("{account}\0{thread}")
}

/// One message of a conversation, as judging it needs.
struct Row {
    id: MessageId,
    mailbox: String,
    message_id: String,
    from: String,
    subject: String,
    signals: u32,
    list_id: String,
    recipients: String,
    has_body: bool,
    flags: u32,
    /// How many of `recipients` were in To (-1: not known).
    to_count: i64,
}

/// A raw message's header section.
fn header_block(raw: &[u8]) -> &[u8] {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 2));
    &raw[..end.unwrap_or(raw.len())]
}

impl Store {
    /// The rules and addresses categories are worked out with from now on. Messages stored
    /// before keep theirs until they're worked out again: at once for the senders of rules
    /// that only name one sender, by a background pass for anything else (see the module docs).
    pub fn set_classifier(&self, next: Classifier) -> Result<()> {
        let (config, pass) = self.category_state()?;
        let fingerprint = next.fingerprint();
        let previous = Classifier::from_fingerprint(&config);
        *self.classifier.borrow_mut() = next;
        if config == fingerprint {
            return Ok(());
        }
        self.transaction(|st| {
            let senders = previous.and_then(|p| p.senders_changed(&st.classifier.borrow()));
            match senders {
                Some(senders) => {
                    st.mark_senders(&senders)?;
                    st.save_category_state(&fingerprint, pass)
                }
                // A new cache has nothing to go over.
                None if !st.conn.query_row("SELECT EXISTS (SELECT 1 FROM messages)", [], |r| r.get::<_, bool>(0))? => st.save_category_state(&fingerprint, None),
                None => st.save_category_state(&fingerprint, Some(i64::MAX)),
            }
        })
    }

    /// A pass over the whole cache is under way ([`Store::recategorize`]).
    pub fn recategorizing(&self) -> Result<bool> {
        Ok(self.category_state()?.1.is_some())
    }

    /// Works out the next `chunk` messages' conversations of a pass over the whole cache, newest
    /// first, in one short transaction (the writer is free between calls).
    pub fn recategorize(&self, chunk: u32) -> Result<Pass> {
        let before = self.recategorized.get();
        let more = self.transaction(|st| {
            let (config, Some(below)) = st.category_state()? else { return Ok(false) };
            if below == i64::MAX {
                st.seed_written_to()?;
            }
            let chunk = chunk.max(1);
            let rows: Vec<(i64, String, String)> = st
                .conn
                .prepare_cached("SELECT id, account, thread FROM messages WHERE id < ?1 ORDER BY id DESC LIMIT ?2")?
                .query_map(params![below, chunk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            st.dirty.borrow_mut().extend(rows.iter().map(|(_, a, t)| key(a, t)));
            let next = if rows.len() == chunk as usize { rows.last().map(|r| r.0) } else { None };
            st.save_category_state(&config, next)?;
            Ok(next.is_some())
        })?;
        Ok(Pass { more, changed: self.recategorized.get() - before })
    }

    /// How many messages changed category since it last asked.
    pub fn take_recategorized(&self) -> usize {
        self.recategorized.replace(0)
    }

    /// Moves conversations (those of `ids`) to `category` by hand: it sticks, whatever their
    /// messages say and whatever comes later. Returns what to give [`Store::restore_categories`]
    /// to take it back.
    pub fn move_to_category(&self, ids: &[MessageId], category: Category) -> Result<Vec<Moved>> {
        self.transaction(|st| {
            let mut moved = Vec::new();
            let mut seen = HashSet::new();
            for id in ids {
                let Some((account, thread)) = st.conn.query_row("SELECT account, thread FROM messages WHERE id = ?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? else { continue };
                if !seen.insert((account.clone(), thread.clone())) {
                    continue;
                }
                let before = st.moved_to(&account, &thread)?;
                st.conn.execute(
                    "INSERT INTO category_overrides (account, thread, category) VALUES (?1, ?2, ?3) ON CONFLICT (account, thread) DO UPDATE SET category = ?3",
                    params![account, thread, category.code()],
                )?;
                st.dirty.borrow_mut().insert(key(&account, &thread));
                moved.push(Moved { account, thread, before });
            }
            Ok(moved)
        })
    }

    /// Takes back [`Store::move_to_category`].
    pub fn restore_categories(&self, moved: &[Moved]) -> Result<()> {
        self.transaction(|st| {
            for m in moved {
                match m.before {
                    Some(c) => st.conn.execute(
                        "INSERT INTO category_overrides (account, thread, category) VALUES (?1, ?2, ?3) ON CONFLICT (account, thread) DO UPDATE SET category = ?3",
                        params![m.account, m.thread, c.code()],
                    )?,
                    None => st.conn.execute("DELETE FROM category_overrides WHERE account = ?1 AND thread = ?2", params![m.account, m.thread])?,
                };
                st.dirty.borrow_mut().insert(key(&m.account, &m.thread));
            }
            Ok(())
        })
    }

    /// Unread conversations (messages, without `conversations`) of each category in a view,
    /// indexed by [`Category::code`]: the inbox's tabs. Reads only unread messages, from
    /// `messages_unread`.
    pub fn category_counts(&self, view: &View, conversations: bool) -> Result<[u32; 4]> {
        let mut counts = [0u32; 4];
        let super::list::Scope::Boxes(boxes) = self.scope(view)? else { return Ok(counts) };
        let mut stmt = self.conn.prepare_cached(COUNT_UNREAD)?;
        let mut seen: HashSet<String> = HashSet::new();
        for (account, mailbox) in &boxes {
            let mut rows = stmt.query(params![account, mailbox])?;
            while let Some(r) = rows.next()? {
                let category: i64 = r.get(0)?;
                if conversations && !seen.insert(key(account, &r.get::<_, String>(1)?)) {
                    continue;
                }
                counts[Category::from_code(category).code() as usize] += 1;
            }
        }
        // On their way in (back to the inbox, say, and not on the server yet): counted where the
        // list shows them.
        let mut arriving = self.conn.prepare_cached("SELECT account, category, thread FROM messages WHERE id = ?1 AND flags & 1 = 0 AND flags & 16 = 0")?;
        for a in self.arrivals_into(&boxes)? {
            let Some((account, category, thread)) = arriving.query_row([a.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).optional()? else { continue };
            if conversations && !seen.insert(key(&account, &thread)) {
                continue;
            }
            counts[Category::from_code(category).code() as usize] += 1;
        }
        Ok(counts)
    }

    /// Message `id`'s conversation's category, and why, in words. Reads; writes nothing.
    pub fn explain(&self, id: MessageId) -> Result<Option<(Category, String)>> {
        let Some((account, thread)) = self.conn.query_row("SELECT account, thread FROM messages WHERE id = ?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? else {
            return Ok(None);
        };
        Ok(self.judge(&account, &thread, &mut Roles::default(), false)?.map(|v| (v.category, v.why.describe())))
    }

    /// A message was stored ([`Store::upsert_header`]): its conversation is marked, and, if you
    /// sent it, its recipients are people you've written to.
    pub(super) fn stored(&self, account: &str, mailbox: &str, thread: &str, from: &str, fl: u32, recipients: &[Address]) -> Result<()> {
        let role = self.role_of(account, mailbox);
        let from_me = self.classifier.borrow().is_me(from) || role == Role::Sent;
        if from_me && role != Role::Drafts && fl & flags::DRAFT == 0 {
            self.note_written_to(recipients)?;
        }
        self.dirty.borrow_mut().insert(key(account, thread));
        self.flush_unless_batched()
    }

    /// A message's body arrived ([`Store::set_body`]): if its headers weren't read when it was
    /// stored (an Envoy from before the split cached it), they are now.
    pub(super) fn headers_arrived(&self, id: MessageId, p: &Parsed) -> Result<()> {
        let row: Option<(u32, String, String)> =
            self.conn.prepare_cached("SELECT signals, account, thread FROM messages WHERE id = ?1")?.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        let Some((known, account, thread)) = row else { return Ok(()) };
        if known & signals::KNOWN != 0 {
            return Ok(());
        }
        self.conn.execute("UPDATE messages SET signals = ?2, list_id = ?3, to_count = ?4 WHERE id = ?1", params![id, category::signals_of(p), p.bulk_headers.list_id, p.to.len()])?;
        self.dirty.borrow_mut().insert(key(&account, &thread));
        self.flush_unless_batched()
    }

    fn flush_unless_batched(&self) -> Result<()> {
        if self.depth.get() == 0 { self.flush_categories() } else { Ok(()) }
    }

    /// Works out the categories of the conversations marked since the last time.
    pub(super) fn flush_categories(&self) -> Result<()> {
        let correspondents = std::mem::take(&mut *self.new_correspondents.borrow_mut());
        self.mark_senders(&correspondents)?;
        let dirty = std::mem::take(&mut *self.dirty.borrow_mut());
        let mut roles = Roles::default();
        for k in &dirty {
            let Some((account, thread)) = k.split_once('\0') else { continue };
            if let Some(v) = self.judge(account, thread, &mut roles, true)? {
                let n = self.conn.prepare_cached(SET_CATEGORY)?.execute(params![account, thread, v.category.code()])?;
                self.recategorized.set(self.recategorized.get() + n);
            }
        }
        Ok(())
    }

    /// What a rolled-back transaction marked.
    pub(super) fn forget_pending(&self) {
        self.dirty.borrow_mut().clear();
        self.new_correspondents.borrow_mut().clear();
        *self.written.borrow_mut() = None;
    }

    /// A conversation's verdict: its messages' (Drafts, Spam and Trash aside, unless that's all
    /// it has), and how it was moved by hand. With `backfill`, messages whose headers weren't
    /// read get them from the downloaded message, where there is one.
    fn judge(&self, account: &str, thread: &str, roles: &mut Roles, backfill: bool) -> Result<Option<Verdict>> {
        let mut rows: Vec<Row> = self
            .conn
            .prepare_cached(THREAD_ROWS)?
            .query_map(params![account, thread], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    mailbox: r.get(1)?,
                    message_id: r.get(2)?,
                    from: r.get(3)?,
                    subject: r.get(4)?,
                    signals: r.get(5)?,
                    list_id: r.get(6)?,
                    recipients: r.get(7)?,
                    has_body: r.get::<_, i64>(8)? != 0,
                    flags: r.get(9)?,
                    to_count: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let moved = self.moved_to(account, thread)?;
        if rows.is_empty() {
            return Ok(None);
        }
        if backfill {
            for row in rows.iter_mut().filter(|r| r.signals & signals::KNOWN == 0 && r.has_body) {
                if let Some((s, list_id, to_count)) = self.signals_from_raw(row.id)? {
                    self.conn.execute("UPDATE messages SET signals = ?2, list_id = ?3, to_count = ?4 WHERE id = ?1", params![row.id, s, list_id, to_count])?;
                    row.signals = s;
                    row.list_id = list_id;
                    row.to_count = to_count as i64;
                }
            }
        }
        let mut row_roles = Vec::with_capacity(rows.len());
        for r in &rows {
            row_roles.push(roles.role(self, account, &r.mailbox)?);
        }
        let gmail: Vec<&str> = rows.iter().zip(&row_roles).filter(|(r, role)| **role == Role::Important && !r.message_id.is_empty()).map(|(r, _)| r.message_id.as_str()).collect();
        let counts = |(r, role): &(&Row, &Role)| !matches!(role, Role::Drafts | Role::Junk | Role::Trash) && r.flags & (flags::DELETED | flags::DRAFT) == 0;
        let any = rows.iter().zip(&row_roles).any(|p| counts(&p));
        let classifier = self.classifier.borrow();
        let mut verdicts = Vec::with_capacity(rows.len());
        for (r, role) in rows.iter().zip(&row_roles).filter(|p| !any || counts(p)) {
            // To you, by your addresses as they are now: the first `to_count` recipients were in
            // To; where that isn't known (cached before the split), any of them.
            let recipients = from_json::<Vec<Address>>(&r.recipients);
            let to = usize::try_from(r.to_count).map_or(recipients.len(), |n| n.min(recipients.len()));
            let facts = Facts {
                from: &r.from,
                subject: &r.subject,
                signals: r.signals,
                list_id: &r.list_id,
                from_me: classifier.is_me(&r.from) || *role == Role::Sent,
                written_to: self.is_written_to(&r.from)?,
                gmail_important: gmail.contains(&r.message_id.as_str()),
                to_me: recipients[..to].iter().any(|a| classifier.is_me(&a.email)),
            };
            verdicts.push(classifier.verdict(&facts));
        }
        Ok(category::conversation(&verdicts, moved))
    }

    fn moved_to(&self, account: &str, thread: &str) -> Result<Option<Category>> {
        Ok(self
            .conn
            .prepare_cached("SELECT category FROM category_overrides WHERE account = ?1 AND thread = ?2")?
            .query_row(params![account, thread], |r| r.get::<_, i64>(0))
            .optional()?
            .map(Category::from_code))
    }

    /// What a downloaded message's headers say, for one stored before they were read: its
    /// signals, list id, and how many of its recipients were in To.
    fn signals_from_raw(&self, id: MessageId) -> Result<Option<(u32, String, usize)>> {
        let raw: Option<Vec<u8>> = self.conn.prepare_cached("SELECT raw FROM bodies WHERE id = ?1")?.query_row([id], |r| r.get(0)).optional()?;
        let Some(p) = raw.as_deref().and_then(|raw| mime::parse(header_block(raw), true)) else { return Ok(None) };
        Ok(Some((category::signals_of(&p), p.bulk_headers.list_id, p.to.len())))
    }

    /// Marks the conversations of mail from these addresses (lowercase).
    fn mark_senders(&self, senders: &[String]) -> Result<()> {
        if senders.is_empty() {
            return Ok(());
        }
        let mut stmt = self.conn.prepare_cached(SENDERS_THREADS)?;
        let mut rows = stmt.query([json(&senders)])?;
        let mut dirty = self.dirty.borrow_mut();
        while let Some(r) = rows.next()? {
            dirty.insert(key(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?));
        }
        Ok(())
    }

    fn note_written_to(&self, recipients: &[Address]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached("INSERT OR IGNORE INTO written_to (email) VALUES (?1)")?;
        for a in recipients {
            let email = a.email.trim().to_lowercase();
            if email.is_empty() || self.classifier.borrow().is_me(&email) {
                continue;
            }
            if stmt.execute([&email])? > 0 {
                if let Some(w) = self.written.borrow_mut().as_mut() {
                    w.insert(email.clone());
                }
                self.new_correspondents.borrow_mut().push(email);
            }
        }
        Ok(())
    }

    fn is_written_to(&self, address: &str) -> Result<bool> {
        if self.written.borrow().is_none() {
            let all: HashSet<String> = self.conn.prepare("SELECT email FROM written_to")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            *self.written.borrow_mut() = Some(all);
        }
        Ok(self.written.borrow().as_ref().is_some_and(|w| w.contains(&address.trim().to_lowercase())))
    }

    /// Everyone the cache's Sent folders show mail to, for a cache from before `written_to`.
    fn seed_written_to(&self) -> Result<()> {
        let added = self.conn.execute(SEED_WRITTEN_TO, [])?;
        if added > 0 {
            *self.written.borrow_mut() = None;
        }
        Ok(())
    }

    fn category_state(&self) -> Result<(String, Option<i64>)> {
        Ok(self.conn.query_row("SELECT config, pass_below FROM category_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or_default())
    }

    fn save_category_state(&self, config: &str, pass_below: Option<i64>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO category_state (id, config, pass_below) VALUES (1, ?1, ?2) ON CONFLICT (id) DO UPDATE SET config = ?1, pass_below = ?2",
            params![config, pass_below],
        )?;
        Ok(())
    }
}

/// A conversation's messages, newest first (ties as lists order them), by the thread index.
pub(super) const THREAD_ROWS: &str = "SELECT id, mailbox, message_id, from_email, subject, signals, list_id, recipients, has_body, flags, to_count FROM messages
    WHERE account = ?1 AND thread = ?2 ORDER BY date DESC, id DESC";
/// Sets a conversation's category, touching only the rows it changes (and so their indexes), by
/// the thread index: left to itself SQLite takes `messages_by_category` (it has every column
/// the condition reads) and walks the whole account for each conversation.
pub(super) const SET_CATEGORY: &str = "UPDATE messages INDEXED BY messages_thread SET category = ?3 WHERE account = ?1 AND thread = ?2 AND category != ?3";
/// The conversations of mail from some addresses, by `messages_from`.
pub(super) const SENDERS_THREADS: &str = "SELECT DISTINCT account, thread FROM messages WHERE lower(from_email) IN (SELECT value FROM json_each(?1))";
/// A mailbox's unread messages' categories, from `messages_unread` (its condition, spelled out,
/// is what lets SQLite use it).
pub(super) const COUNT_UNREAD: &str = "SELECT category, thread FROM messages INDEXED BY messages_unread
    WHERE account = ?1 AND mailbox = ?2 AND flags & 1 = 0 AND flags & 16 = 0 AND held_op = 0";
const SEED_WRITTEN_TO: &str = "INSERT OR IGNORE INTO written_to (email)
    SELECT DISTINCT lower(trim(json_extract(r.value, '$.email')))
    FROM mailboxes b JOIN messages m ON m.account = b.account AND m.mailbox = b.name JOIN json_each(m.recipients) r
    WHERE b.role = 'sent' AND m.flags & 8 = 0 AND trim(COALESCE(json_extract(r.value, '$.email'), '')) != ''";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::Rule;
    use crate::store::Query;
    use rusqlite::Connection;

    const ME: &str = "me@x.example";

    fn classifier(rules: Vec<Rule>) -> Classifier {
        Classifier::new(rules, [ME.to_owned()])
    }

    /// A Gmail-like account: Inbox, Sent, Important, Trash; your address set.
    fn store() -> Store {
        let st = Store::memory();
        for (name, role) in [("INBOX", Role::Inbox), ("Sent", Role::Sent), ("[Gmail]/Important", Role::Important), ("Trash", Role::Trash)] {
            st.upsert_mailbox("a", name, name, role, "/", true).unwrap();
        }
        st.set_classifier(classifier(vec![])).unwrap();
        assert!(!st.recategorizing().unwrap(), "nothing to go over in a new cache");
        st
    }

    fn raw(headers: &str, minute: u32) -> String {
        format!("{headers}\r\nDate: Tue, 3 Mar 2026 10:{minute:02}:00 +0000\r\n\r\nbody\r\n")
    }

    fn put(st: &Store, mailbox: &str, uid: u32, headers: &str) -> MessageId {
        let p = mime::parse(raw(headers, uid % 60).as_bytes(), true).unwrap();
        st.upsert_header("a", mailbox, uid, 0, 10, 0, &p).unwrap()
    }

    fn cat(st: &Store, id: MessageId) -> Category {
        st.summary(id).unwrap().unwrap().category
    }

    fn finish_pass(st: &Store) {
        while st.recategorize(3).unwrap().more {}
    }

    const NEWSLETTER: &str = "From: Northwind Weekly <hello@northwind.example>\r\nTo: me@x.example\r\nSubject: This week\r\nMessage-ID: <news1@northwind.example>\r\nList-Unsubscribe: <https://northwind.example/u>";
    const GITHUB: &str = "From: Jordan <notifications@github.com>\r\nTo: x/y <y@noreply.github.com>\r\nCc: me@x.example\r\nSubject: [x/y] PR #42\r\nMessage-ID: <pr42@github.com>\r\nList-ID: x/y <y.x.github.com>\r\nX-GitHub-Reason: review_requested";
    const DIRECT: &str = "From: Sam <sam@ortiz.family>\r\nTo: me@x.example\r\nSubject: Dinner?\r\nMessage-ID: <dinner@ortiz.family>";
    const COPIED: &str = "From: Bo <bo@northwind.dev>\r\nTo: team@northwind.dev\r\nCc: me@x.example\r\nSubject: Offsite agenda\r\nMessage-ID: <offsite@northwind.dev>";

    #[test]
    fn messages_are_sorted_as_they_land() {
        let st = store();
        let (news, github, direct, copied) = (put(&st, "INBOX", 1, NEWSLETTER), put(&st, "INBOX", 2, GITHUB), put(&st, "INBOX", 3, DIRECT), put(&st, "INBOX", 4, COPIED));
        assert_eq!([cat(&st, news), cat(&st, github), cat(&st, direct), cat(&st, copied)], [Category::Newsletters, Category::Notifications, Category::Important, Category::Other]);
        assert_eq!(st.explain(news).unwrap(), Some((Category::Newsletters, "Has an unsubscribe link".into())));
        assert_eq!(st.explain(copied).unwrap().unwrap().1, "You're copied, or it went to a group");
        // In a download's transaction, the same.
        let ids = st.transaction(|st| Ok([put(st, "INBOX", 11, &NEWSLETTER.replace("news1", "news2")), put(st, "INBOX", 12, &DIRECT.replace("dinner@", "lunch@"))])).unwrap();
        assert_eq!([cat(&st, ids[0]), cat(&st, ids[1])], [Category::Newsletters, Category::Important]);
    }

    #[test]
    fn a_conversation_you_reply_to_is_important() {
        let st = store();
        let news = put(&st, "INBOX", 1, NEWSLETTER);
        assert_eq!(cat(&st, news), Category::Newsletters);
        // You reply (the Sent copy lands later, as the sync of Sent does).
        let reply = put(&st, "Sent", 1, "From: Me <me@x.example>\r\nTo: hello@northwind.example\r\nSubject: Re: This week\r\nMessage-ID: <r1@x.example>\r\nIn-Reply-To: <news1@northwind.example>\r\nReferences: <news1@northwind.example>");
        assert_eq!((cat(&st, news), cat(&st, reply)), (Category::Important, Category::Important), "the whole conversation");
        assert_eq!(st.explain(news).unwrap().unwrap().1, "You're part of this conversation");
        // A draft of a reply isn't taking part.
        let other = put(&st, "INBOX", 2, &NEWSLETTER.replace("news1", "news9"));
        let p = mime::parse(raw("From: me@x.example\r\nTo: hello@northwind.example\r\nSubject: Re\r\nMessage-ID: <d@x.example>\r\nReferences: <news9@northwind.example>", 5).as_bytes(), true).unwrap();
        st.upsert_header("a", "INBOX", 3, flags::DRAFT, 10, 0, &p).unwrap();
        assert_eq!(cat(&st, other), Category::Newsletters);
    }

    #[test]
    fn writing_to_someone_brings_their_mail_forward() {
        let st = store();
        let copied = put(&st, "INBOX", 1, COPIED);
        assert_eq!(cat(&st, copied), Category::Other);
        // You write to Bo, in another conversation: Bo's mail, old and new, is Important.
        put(&st, "Sent", 1, "From: me@x.example\r\nTo: Bo <Bo@Northwind.dev>\r\nSubject: Coffee?\r\nMessage-ID: <c@x.example>");
        assert_eq!(cat(&st, copied), Category::Important);
        assert_eq!(st.explain(copied).unwrap().unwrap().1, "From someone you've written to");
        let later = put(&st, "INBOX", 2, &COPIED.replace("offsite@", "later@"));
        assert_eq!(cat(&st, later), Category::Important);
    }

    #[test]
    fn gmails_important_label_counts_when_it_arrives() {
        let st = store();
        let copied = put(&st, "INBOX", 1, COPIED);
        assert_eq!(cat(&st, copied), Category::Other);
        // The same message under Important, synced after the inbox.
        put(&st, "[Gmail]/Important", 1, COPIED);
        assert_eq!(cat(&st, copied), Category::Important);
        assert_eq!(st.explain(copied).unwrap().unwrap().1, "Marked important by Gmail");
    }

    #[test]
    fn rules_move_mail_at_once_or_in_a_pass_and_corrections_stick() {
        let st = store();
        let (news, github, direct) = (put(&st, "INBOX", 1, NEWSLETTER), put(&st, "INBOX", 2, GITHUB), put(&st, "INBOX", 3, DIRECT));
        // A correction's rule (one sender): its mail moves at once, no pass.
        st.set_classifier(classifier(vec![Rule::sender("hello@northwind.example", Category::Important)])).unwrap();
        assert!(!st.recategorizing().unwrap());
        assert_eq!(cat(&st, news), Category::Important);
        assert_eq!(st.explain(news).unwrap().unwrap().1, "Your rule: from:hello@northwind.example important");
        // A domain rule: a pass, in the background; until it gets there, mail stays where it was.
        let github_rule = Rule { from: "*@github.com".into(), list: String::new(), category: Category::Other };
        st.set_classifier(classifier(vec![Rule::sender("hello@northwind.example", Category::Important), github_rule.clone()])).unwrap();
        assert!(st.recategorizing().unwrap());
        assert_eq!(cat(&st, github), Category::Notifications);
        let mut changed = 0;
        loop {
            let pass = st.recategorize(1).unwrap();
            changed += pass.changed;
            if !pass.more {
                break;
            }
        }
        assert_eq!((cat(&st, github), changed), (Category::Other, 1), "only what moved is written");
        // Mail stored meanwhile is sorted with the new rules straight away.
        let next = put(&st, "INBOX", 4, &GITHUB.replace("pr42@", "pr43@"));
        assert_eq!(cat(&st, next), Category::Other);
        // A correction beats rules and heuristics, and is taken back exactly.
        let moved = st.move_to_category(&[direct, news, direct], Category::Newsletters).unwrap();
        assert_eq!(moved.len(), 2, "once per conversation");
        assert_eq!((cat(&st, direct), cat(&st, news)), (Category::Newsletters, Category::Newsletters));
        let again = st.move_to_category(&[direct], Category::Notifications).unwrap();
        assert_eq!(again[0].before, Some(Category::Newsletters));
        st.restore_categories(&again).unwrap();
        assert_eq!(cat(&st, direct), Category::Newsletters);
        st.restore_categories(&moved).unwrap();
        assert_eq!((cat(&st, direct), cat(&st, news)), (Category::Important, Category::Important));
        // Taking the rule back moves its sender's mail back.
        st.set_classifier(classifier(vec![github_rule])).unwrap();
        assert!(!st.recategorizing().unwrap());
        assert_eq!(cat(&st, news), Category::Newsletters);
    }

    /// Whether mail is to you depends on your addresses: one added later (another account),
    /// or removed, re-sorts the mail sent to it.
    #[test]
    fn adding_or_removing_an_address_re_sorts_mail_to_it() {
        let st = store();
        let alias = put(&st, "INBOX", 1, "From: Sam <sam@ortiz.family>\r\nTo: Me <ME@alias.example>\r\nSubject: Hi\r\nMessage-ID: <h@x>");
        let copied = put(&st, "INBOX", 2, "From: Bo <bo@x.example>\r\nTo: team@x.example\r\nCc: me@alias.example\r\nSubject: Team\r\nMessage-ID: <t@x>");
        assert_eq!((cat(&st, alias), cat(&st, copied)), (Category::Other, Category::Other), "not yours yet");
        st.set_classifier(Classifier::new(vec![], [ME.to_owned(), "me@alias.example".to_owned()])).unwrap();
        finish_pass(&st);
        assert_eq!((cat(&st, alias), cat(&st, copied)), (Category::Important, Category::Other), "to you, and only copied");
        st.set_classifier(classifier(vec![])).unwrap();
        finish_pass(&st);
        assert_eq!(cat(&st, alias), Category::Other, "yours no longer");
    }

    #[test]
    fn tab_counts_are_unread_conversations_per_category() {
        let st = store();
        put(&st, "INBOX", 1, NEWSLETTER);
        put(&st, "INBOX", 2, &NEWSLETTER.replace("news1", "news2"));
        let direct = put(&st, "INBOX", 3, DIRECT);
        // A second unread message of the same conversation.
        put(&st, "INBOX", 4, "From: Sam <sam@ortiz.family>\r\nTo: me@x.example\r\nSubject: Re: Dinner?\r\nMessage-ID: <d2@ortiz.family>\r\nReferences: <dinner@ortiz.family>");
        let read = put(&st, "INBOX", 5, COPIED);
        st.change_flags(read, flags::SEEN, 0).unwrap();
        let view = View::Role { account: None, role: Role::Inbox };
        let at = |c: Category| c.code() as usize;
        let counts = st.category_counts(&view, true).unwrap();
        assert_eq!((counts[at(Category::Newsletters)], counts[at(Category::Important)], counts[at(Category::Other)]), (2, 1, 0));
        assert_eq!(st.category_counts(&view, false).unwrap()[at(Category::Important)], 2, "messages");
        st.change_flags(direct, flags::SEEN, 0).unwrap();
        assert_eq!(st.category_counts(&view, false).unwrap()[at(Category::Important)], 1);
        // A tab's list, and search across tabs unless it asks.
        let me: HashSet<String> = [ME.to_owned()].into();
        let tab = |c| st.list(&view, &Query::in_category(c), true, 100, &me).unwrap();
        assert_eq!(tab(Category::Newsletters).len(), 2);
        assert_eq!(tab(Category::Important).len(), 1);
        assert_eq!(tab(Category::Notifications).len(), 0);
        assert_eq!(st.list(&view, &Query::parse("from:northwind"), true, 100, &me).unwrap().len(), 3, "every category");
        assert_eq!(st.list(&view, &Query::parse("from:northwind category:news"), true, 100, &me).unwrap().len(), 2);
        assert_eq!(st.list(&view, &Query::parse("category:o"), true, 100, &me).unwrap().len(), 1, "Other");
        assert_eq!(st.list(&view, &Query::parse("category:zzz"), true, 100, &me).unwrap().len(), 0, "names none");
        assert_eq!(st.list(&view, &Query::parse("category:"), true, 100, &me).unwrap().len(), 4, "as it's typed: everything");
    }

    #[test]
    fn category_search_reads_like_the_other_operators() {
        assert_eq!(Query::parse("category:Newsletters").category, Some(1 << Category::Newsletters.code()));
        assert_eq!(Query::parse("category:n").category, Some(1 << Category::Newsletters.code() | 1 << Category::Notifications.code()));
        assert_eq!(Query::parse("category:notification").category, Some(1 << Category::Notifications.code()));
        assert_eq!(Query::parse("category:zz").category, Some(0));
        assert_eq!(Query::parse("category:n category:news").category, Some(1 << Category::Newsletters.code()), "both apply");
        assert_eq!(Query::parse("category:").category, None);
        assert_eq!(Query::in_category(Category::Other).categories(), Some(1));
        assert!(!Query::parse("category:other").is_empty());
    }

    #[test]
    fn caches_from_before_the_split_are_sorted_in_a_pass_that_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("envoy-store-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        {
            let old = super::super::SCHEMA
                .replace("    category INTEGER NOT NULL DEFAULT 0,\n", "")
                .replace("    signals INTEGER NOT NULL DEFAULT 0,\n", "")
                .replace("    list_id TEXT NOT NULL DEFAULT '',\n", "");
            assert!(!old.contains("signals INTEGER"), "set up a cache from before");
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(&old).unwrap();
            conn.execute_batch(
                "INSERT INTO mailboxes (account, name, display, role) VALUES ('a', 'INBOX', 'Inbox', 'inbox'), ('a', 'Sent', 'Sent', 'sent');
                 -- 1: a newsletter, downloaded (its headers are in the message).
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, from_email, recipients, date, has_body)
                   VALUES (1, 'a', 'INBOX', 1, '<n@x>', '<n@x>', 'This week', 'hello@northwind.example', '[{\"name\":\"\",\"email\":\"me@x.example\"}]', 1, 1);
                 -- 2: a person writing to you, not downloaded: judged by its recipients.
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, from_email, recipients, date)
                   VALUES (2, 'a', 'INBOX', 2, '<s@x>', '<s@x>', 'Dinner?', 'sam@ortiz.family', '[{\"name\":\"\",\"email\":\"me@x.example\"}]', 2);
                 -- 3: copied, from someone your Sent folder shows you wrote to.
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, from_email, recipients, date)
                   VALUES (3, 'a', 'INBOX', 3, '<o@x>', '<o@x>', 'Offsite', 'bo@northwind.dev', '[{\"name\":\"\",\"email\":\"team@northwind.dev\"}]', 3);
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, from_email, recipients, date)
                   VALUES (4, 'a', 'Sent', 1, '<c@x>', '<c@x>', 'Coffee?', 'me@x.example', '[{\"name\":\"Bo\",\"email\":\"Bo@northwind.dev\"}]', 4);
                 -- 5: a receipt, not downloaded.
                 INSERT INTO messages (id, account, mailbox, uid, message_id, thread, subject, from_email, recipients, date)
                   VALUES (5, 'a', 'INBOX', 5, '<r@x>', '<r@x>', 'Your receipt', 'receipts@stripe.com', '[]', 5);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO bodies (id, raw) VALUES (1, ?1)",
                [raw("From: hello@northwind.example\r\nTo: me@x.example\r\nSubject: This week\r\nList-Unsubscribe: <https://x.example/u>", 1).into_bytes()],
            )
            .unwrap();
        }
        let ids = [1, 2, 3, 5];
        {
            let st = Store::open(&db).unwrap();
            assert!(ids.iter().all(|id| cat(&st, *id) == Category::Other), "until sorted, in Other: never nowhere");
            st.set_classifier(classifier(vec![])).unwrap();
            assert!(st.recategorizing().unwrap());
            assert!(st.recategorize(2).unwrap().more, "part way");
        }
        let st = Store::open(&db).unwrap();
        st.set_classifier(classifier(vec![])).unwrap();
        assert!(st.recategorizing().unwrap(), "carries on after a restart");
        finish_pass(&st);
        assert_eq!(ids.map(|id| cat(&st, id)), [Category::Newsletters, Category::Important, Category::Important, Category::Notifications]);
        let signals: u32 = st.conn.query_row("SELECT signals FROM messages WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(signals & (signals::KNOWN | signals::LIST_UNSUBSCRIBE), signals::KNOWN | signals::LIST_UNSUBSCRIBE, "read from the message");
        // A body arriving later brings its headers.
        let p = mime::parse(raw("From: sam@ortiz.family\r\nTo: team@x.example\r\nCc: me@x.example\r\nSubject: Dinner?\r\nPrecedence: bulk", 2).as_bytes(), false).unwrap();
        st.set_body(2, b"raw", &p).unwrap();
        assert_eq!(cat(&st, 2), Category::Newsletters);
        drop(st);
        let st = Store::open(&db).unwrap();
        st.set_classifier(classifier(vec![])).unwrap();
        assert!(!st.recategorizing().unwrap(), "nothing left to do");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
