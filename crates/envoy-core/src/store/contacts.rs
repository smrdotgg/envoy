//! The people in the cached mail, as recipient suggestions need them (see [`crate::contacts`]
//! for how they're ranked and matched).
//!
//! `contacts` has a row per account and address: its counts ([`Stats`]), and the name to show.
//! `contact_terms` is what lookups search: each contact's [`terms`], so typing `jo` reads the
//! index's `jo…` range and nothing else ([`SUGGEST`]). `contact_sources` remembers the messages
//! already counted, by Message-ID, so a message is counted once however many copies the cache
//! holds or has held: in two mailboxes (Gmail's labels), or archived (its row goes when the
//! move replays, and the Archive's copy comes later with a new UID).
//!
//! A message's people are counted as it's first stored ([`Store::upsert_header`]); messages
//! cached by a build from before, once, in the background, a chunk at a time
//! ([`Store::backfill_contacts`]). Nothing is ever subtracted: someone you wrote to stays a
//! contact after the mail leaves the cache's window. Removing the account removes them.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::Store;
use crate::contacts::{self, Candidate, Stats, source};
use crate::model::{Address, Role, flags};

pub(super) const CONTACTS: &str = "
CREATE TABLE contacts (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    email TEXT NOT NULL,
    name TEXT NOT NULL DEFAULT '',
    name_src INTEGER NOT NULL DEFAULT 0,
    name_at INTEGER NOT NULL DEFAULT 0,
    sent INTEGER NOT NULL DEFAULT 0,
    sent_at INTEGER NOT NULL DEFAULT 0,
    got INTEGER NOT NULL DEFAULT 0,
    cc INTEGER NOT NULL DEFAULT 0,
    bulk INTEGER NOT NULL DEFAULT 0,
    seen_at INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account, email)
);
CREATE TABLE contact_terms (
    term TEXT NOT NULL,
    contact INTEGER NOT NULL,
    PRIMARY KEY (term, contact)
) WITHOUT ROWID;
CREATE INDEX contact_terms_contact ON contact_terms (contact);
CREATE TRIGGER contacts_terms_delete AFTER DELETE ON contacts BEGIN
    DELETE FROM contact_terms WHERE contact = old.id;
END;
CREATE TABLE contact_sources (
    account TEXT NOT NULL,
    message INTEGER NOT NULL,
    PRIMARY KEY (account, message)
) WITHOUT ROWID;
";

/// Messages cached before contacts were kept, by id: `next` up to `upto` are still to count.
/// Messages stored after it was made (ids above `upto`) are counted as they come.
pub(super) const BACKFILL: &str = "CREATE TABLE contacts_backfill (next INTEGER NOT NULL, upto INTEGER NOT NULL)";

/// A lookup: the contacts with a term in the typed word's range (`?1` ≤ term < `?2`, the
/// word followed by the highest character), read through contact_terms' primary key.
pub(super) const SUGGEST: &str = "SELECT email, name, name_src, name_at, sent, sent_at, got, cc, bulk, seen_at FROM contacts
    WHERE id IN (SELECT contact FROM contact_terms WHERE term >= ?1 AND term < ?2)";

/// One message, as far as its people go.
pub(super) struct Mail<'a> {
    pub message_id: &'a str,
    pub from: &'a Address,
    /// To and Cc.
    pub recipients: &'a [Address],
    pub bcc: &'a [Address],
    pub bulk: bool,
    pub date: i64,
    pub flags: u32,
}

/// What one message adds to one address.
#[derive(Default)]
struct Seen {
    stats: Stats,
    name: String,
    name_src: i64,
}

/// How much of a message the backfill reads, at most, to find its headers: a few KB at a time,
/// until they end.
const HEADER_PEEK: usize = 32 * 1024;
const PEEK_STEP: usize = 4 * 1024;

impl Store {
    /// Your addresses (lowercase): the mail they send is yours, and they're never suggested.
    /// The engine sets them as accounts come and go.
    pub fn set_me(&self, me: HashSet<String>) {
        *self.me.borrow_mut() = me;
    }

    /// Counts a message's people (once per Message-ID: see the module docs). Drafts and spam
    /// count for nothing. Call inside a transaction.
    pub(super) fn add_people(&self, account: &str, mailbox: &str, m: &Mail) -> Result<()> {
        if m.flags & flags::DRAFT != 0 {
            return Ok(());
        }
        let role = self.role_of(account, mailbox);
        if matches!(role, Role::Drafts | Role::Junk) {
            return Ok(());
        }
        if !m.message_id.trim().is_empty() {
            let new = self.conn.prepare_cached("INSERT OR IGNORE INTO contact_sources (account, message) VALUES (?1, ?2)")?.execute(params![account, contacts::message_hash(m.message_id)])?;
            if new == 0 {
                return Ok(());
            }
        }
        let me = self.me.borrow();
        let mine = |a: &Address| me.contains(&a.email.trim().to_lowercase());
        let mut seen: Vec<(String, Seen)> = Vec::new();
        let mut note = |a: &Address, name_src: i64, f: &dyn Fn(&mut Stats)| {
            let email = a.email.trim().to_lowercase();
            if !contacts::plausible(&email) || me.contains(&email) {
                return;
            }
            let i = match seen.iter().position(|(e, _)| *e == email) {
                Some(i) => i,
                None => {
                    seen.push((email, Seen::default()));
                    seen.len() - 1
                }
            };
            let s = &mut seen[i].1;
            f(&mut s.stats);
            let name = contacts::clean_name(&a.name);
            if !name.is_empty() && name_src > s.name_src {
                s.name = name;
                s.name_src = name_src;
            }
        };
        let date = m.date;
        if role == Role::Sent || mine(m.from) {
            for a in m.recipients.iter().chain(m.bcc) {
                note(a, source::YOURS, &|s| {
                    s.sent = 1;
                    s.sent_at = date;
                });
            }
        } else {
            let bulk = m.bulk;
            note(m.from, source::THEIRS, &|s| {
                if bulk { s.bulk = 1 } else { s.got = 1 }
                s.seen_at = date;
            });
            // Who else a list or a newsletter went to isn't anyone you know.
            if !bulk {
                for a in m.recipients {
                    note(a, source::COPIED, &|s| {
                        if s.got == 0 {
                            s.cc = 1;
                        }
                        s.seen_at = date;
                    });
                }
            }
        }
        drop(me);
        for (email, s) in seen {
            self.add_contact(account, &email, &s, date)?;
        }
        Ok(())
    }

    fn add_contact(&self, account: &str, email: &str, s: &Seen, date: i64) -> Result<()> {
        let have: Option<(i64, String, i64, i64)> = self
            .conn
            .prepare_cached("SELECT id, name, name_src, name_at FROM contacts WHERE account = ?1 AND email = ?2")?
            .query_row(params![account, email], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let t = &s.stats;
        let Some((id, name, name_src, name_at)) = have else {
            self.conn
                .prepare_cached(
                    "INSERT INTO contacts (account, email, name, name_src, name_at, sent, sent_at, got, cc, bulk, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )?
                .execute(params![account, email, s.name, s.name_src, date, t.sent, t.sent_at, t.got, t.cc, t.bulk, t.seen_at])?;
            let id = self.conn.last_insert_rowid();
            return self.write_terms(id, &s.name, email);
        };
        let rename = !s.name.is_empty() && (s.name_src, date) >= (name_src, name_at);
        let (new_name, new_src, new_at) = if rename { (s.name.as_str(), s.name_src, date) } else { (name.as_str(), name_src, name_at) };
        self.conn
            .prepare_cached(
                "UPDATE contacts SET sent = sent + ?2, sent_at = MAX(sent_at, ?3), got = got + ?4, cc = cc + ?5, bulk = bulk + ?6, seen_at = MAX(seen_at, ?7),
                 name = ?8, name_src = ?9, name_at = ?10 WHERE id = ?1",
            )?
            .execute(params![id, t.sent, t.sent_at, t.got, t.cc, t.bulk, t.seen_at, new_name, new_src, new_at])?;
        if new_name != name {
            self.conn.prepare_cached("DELETE FROM contact_terms WHERE contact = ?1")?.execute(params![id])?;
            self.write_terms(id, new_name, email)?;
        }
        Ok(())
    }

    fn write_terms(&self, id: i64, name: &str, email: &str) -> Result<()> {
        let mut insert = self.conn.prepare_cached("INSERT OR IGNORE INTO contact_terms (term, contact) VALUES (?1, ?2)")?;
        for term in contacts::terms(name, email) {
            insert.execute(params![term, id])?;
        }
        Ok(())
    }

    /// Up to `limit` people to suggest for what's typed in a recipients field, best first
    /// ([`contacts::rank`]): across accounts, never `me` (lowercase addresses). Reads only
    /// the contacts with a word starting with the longest word typed.
    pub fn suggest(&self, typed: &str, me: &HashSet<String>, now: i64, limit: usize) -> Result<Vec<Address>> {
        let words = contacts::query_words(typed);
        let Some(probe) = contacts::probe(&words) else { return Ok(Vec::new()) };
        let end = format!("{probe}\u{10FFFF}");
        let mut stmt = self.conn.prepare_cached(SUGGEST)?;
        let rows = stmt.query_map(params![probe, end], |r| {
            Ok(Candidate {
                email: r.get(0)?,
                name: r.get(1)?,
                name_rank: (r.get(2)?, r.get(3)?),
                stats: Stats { sent: r.get(4)?, sent_at: r.get(5)?, got: r.get(6)?, cc: r.get(7)?, bulk: r.get(8)?, seen_at: r.get(9)? },
            })
        })?;
        let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(contacts::rank(candidates, &words, me, now, limit))
    }

    /// Counts the people of the next `chunk` messages (by id) of a cache from before contacts
    /// were kept ([`BACKFILL`]), or finishes once it's through. False when there's nothing
    /// left to do. Each call is one short transaction, so the writer is free between.
    ///
    /// A message's people come from its row; whether it was bulk, and its Bcc, from its
    /// headers, when it's downloaded (only their first 32 KB are read).
    pub fn backfill_contacts(&self, chunk: i64) -> Result<bool> {
        if !self.contacts_backfill.get() {
            return Ok(false);
        }
        let more = self.transaction(|st| {
            let Some((next, upto)) = st.conn.query_row("SELECT next, upto FROM contacts_backfill", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).optional()? else {
                st.conn.execute_batch("DROP TABLE contacts_backfill")?;
                return Ok(false);
            };
            if next > upto {
                st.conn.execute_batch("DROP TABLE contacts_backfill")?;
                return Ok(false);
            }
            let end = next.saturating_add(chunk.max(1)).min(upto + 1);
            type Row = (i64, String, String, String, String, String, String, i64, u32, bool);
            let rows: Vec<Row> = st
                .conn
                .prepare_cached(
                    "SELECT m.id, m.account, m.mailbox, m.message_id, m.from_name, m.from_email, m.recipients, m.date, m.flags, EXISTS (SELECT 1 FROM bodies b WHERE b.id = m.id)
                     FROM messages m WHERE m.id >= ?1 AND m.id < ?2 ORDER BY m.id",
                )?
                .query_map(params![next, end], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, account, mailbox, message_id, from_name, from_email, recipients, date, fl, downloaded) in rows {
                let (bcc, bulk) = if downloaded { st.peek_headers(id)? } else { (Vec::new(), false) };
                let recipients: Vec<Address> = super::from_json(&recipients);
                let from = Address { name: from_name, email: from_email };
                st.add_people(&account, &mailbox, &Mail { message_id: &message_id, from: &from, recipients: &recipients, bcc: &bcc, bulk, date, flags: fl })?;
            }
            st.conn.execute("UPDATE contacts_backfill SET next = ?1", params![end])?;
            Ok(true)
        })?;
        if !more {
            self.contacts_backfill.set(false);
        }
        Ok(more)
    }

    /// A downloaded message's Bcc and whether it's bulk, from its headers, read off the start
    /// of its raw bytes without loading the rest (attachments can be megabytes).
    fn peek_headers(&self, id: i64) -> Result<(Vec<Address>, bool)> {
        use std::io::Read;
        let Ok(mut blob) = self.conn.blob_open(rusqlite::MAIN_DB, "bodies", "raw", id, true) else { return Ok((Vec::new(), false)) };
        let ends = |b: &[u8]| b.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).or_else(|| b.windows(2).position(|w| w == b"\n\n").map(|i| i + 2));
        let mut head = Vec::with_capacity(PEEK_STEP);
        let mut step = [0; PEEK_STEP];
        let end = loop {
            let n = blob.read(&mut step)?;
            // The blank line may straddle two reads.
            let from = head.len().saturating_sub(3);
            head.extend_from_slice(&step[..n]);
            if let Some(i) = ends(&head[from..]) {
                break from + i;
            }
            if n == 0 || head.len() >= HEADER_PEEK {
                break head.len();
            }
        };
        Ok(crate::mime::parse(&head[..end], true).map(|p| (p.bcc, p.bulk)).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mime;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;
    const ME: &str = "me@me.test";

    fn me() -> HashSet<String> {
        [ME.to_owned()].into()
    }

    fn store() -> Store {
        let s = Store::memory();
        boxes(&s);
        s
    }

    fn boxes(s: &Store) {
        for (name, role) in [("INBOX", Role::Inbox), ("Sent", Role::Sent), ("Archive", Role::Archive), ("Label", Role::Other), ("Junk", Role::Junk), ("Drafts", Role::Drafts)] {
            s.upsert_mailbox("a", name, name, role, "/", true).unwrap();
        }
        s.set_me(me());
    }

    /// A message's raw headers: `days_ago` before NOW, with any `extra` header lines.
    fn raw(from: &str, to: &str, cc: &str, mid: &str, days_ago: i64, extra: &str) -> String {
        let date = chrono::DateTime::from_timestamp(NOW - days_ago * DAY, 0).unwrap().to_rfc2822();
        let cc = if cc.is_empty() { String::new() } else { format!("Cc: {cc}\r\n") };
        format!("From: {from}\r\nTo: {to}\r\n{cc}Subject: hi\r\nMessage-ID: {mid}\r\nDate: {date}\r\n{extra}\r\nbody\r\n")
    }

    /// Stores a message as the sync does, its body too; returns its id.
    fn put(s: &Store, mailbox: &str, uid: u32, raw: &str) -> i64 {
        let p = mime::parse(raw.as_bytes(), false).unwrap();
        let id = s.upsert_header("a", mailbox, uid, flags::SEEN, raw.len() as u32, 0, &p).unwrap();
        s.set_body(id, raw.as_bytes(), &p).unwrap();
        id
    }

    fn emails(s: &Store, typed: &str) -> Vec<String> {
        s.suggest(typed, &me(), NOW, 8).unwrap().into_iter().map(|a| a.email).collect()
    }

    fn row(s: &Store, email: &str) -> (u32, u32, u32, u32) {
        s.conn.query_row("SELECT sent, got, cc, bulk FROM contacts WHERE email = ?1", [email], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
    }

    #[test]
    fn people_you_write_to_come_first_then_those_who_write_to_you_then_bulk() {
        let s = store();
        let mut uid = 0;
        let mut next = || {
            uid += 1;
            uid
        };
        for i in 0..6 {
            put(&s, "INBOX", next(), &raw("Ana Lima <ana@x.com>", ME, "", &format!("<ana{i}@x>"), 2, ""));
        }
        put(&s, "Sent", next(), &raw(&format!("Me <{ME}>"), "Alex Kim <alex@y.org>", "", "<alex@x>", 3, ""));
        for i in 0..20 {
            put(&s, "INBOX", next(), &raw("Al's Deals <deals@al.example>", ME, "", &format!("<deal{i}@x>"), 1, "List-Unsubscribe: <mailto:u@al.example>\r\n"));
        }
        put(&s, "INBOX", next(), &raw("Alerts <noreply@alerts.example>", ME, "", "<alert@x>", 0, ""));
        put(&s, "INBOX", next(), &raw("Me <me@me.test>", "Me <me@me.test>", "", "<self@x>", 0, ""));
        assert_eq!(emails(&s, "a"), vec!["alex@y.org", "ana@x.com", "deals@al.example"], "no-reply and my own address left out");
        assert_eq!(s.suggest("a", &me(), NOW, 1).unwrap(), vec![Address { name: "Alex Kim".into(), email: "alex@y.org".into() }]);
        // A newsletter you've written to (to unsubscribe, say) is someone you write to.
        put(&s, "Sent", 99, &raw(ME, "deals@al.example", "", "<unsub@x>", 0, ""));
        assert_eq!(emails(&s, "a")[0], "deals@al.example");
    }

    #[test]
    fn recent_correspondents_come_first() {
        let s = store();
        put(&s, "Sent", 1, &raw(ME, "Sam Old <sam.old@x.com>", "", "<1@x>", 400, ""));
        put(&s, "Sent", 2, &raw(ME, "Sam New <sam.new@x.com>", "", "<2@x>", 1, ""));
        assert_eq!(emails(&s, "sam"), vec!["sam.new@x.com", "sam.old@x.com"]);
    }

    #[test]
    fn typing_finds_names_addresses_and_ignores_case_and_accents() {
        let s = store();
        put(&s, "INBOX", 1, &raw("Jordan Lee <jl@x.com>", ME, "\"Zoë Müller\" <zm@y.org>", "<1@x>", 1, ""));
        put(&s, "INBOX", 2, &raw("john.smith@z.net", ME, "", "<2@x>", 2, ""));
        assert_eq!(emails(&s, "jo"), vec!["jl@x.com", "john.smith@z.net"]);
        assert_eq!(emails(&s, "lee"), vec!["jl@x.com"]);
        assert_eq!(emails(&s, "JL@X"), vec!["jl@x.com"]);
        assert_eq!(emails(&s, "smi"), vec!["john.smith@z.net"]);
        assert_eq!(emails(&s, "zoe"), vec!["zm@y.org"]);
        assert_eq!(emails(&s, "MÜLL"), vec!["zm@y.org"]);
        assert_eq!(emails(&s, "jordan l"), vec!["jl@x.com"]);
        assert!(emails(&s, "jordan s").is_empty());
        assert!(emails(&s, "x.com").is_empty(), "not by domain");
        assert!(emails(&s, "  ").is_empty());
    }

    #[test]
    fn each_message_counts_once_however_many_copies() {
        let s = store();
        let m = raw("Ana <ana@x.com>", ME, "Bo <bo@x.com>", "<1@x>", 1, "");
        let id = put(&s, "INBOX", 1, &m);
        put(&s, "Label", 7, &m); // the same message under a Gmail label
        // A sync refreshing its flags.
        s.upsert_header("a", "INBOX", 1, 0, 10, 0, &mime::parse(m.as_bytes(), true).unwrap()).unwrap();
        assert_eq!(row(&s, "ana@x.com"), (0, 1, 0, 0));
        assert_eq!(row(&s, "bo@x.com"), (0, 0, 1, 0), "copied on mail to me");
        // Archived: the inbox row goes when the move replays; the Archive's copy comes later.
        s.remove_local(id).unwrap();
        put(&s, "Archive", 3, &m);
        assert_eq!(row(&s, "ana@x.com"), (0, 1, 0, 0), "archiving doesn't count it again");
        // Messages without a Message-ID can't be told apart: each counts.
        put(&s, "INBOX", 4, &raw("Ana <ana@x.com>", ME, "", "", 1, ""));
        assert_eq!(row(&s, "ana@x.com").1, 2);
    }

    #[test]
    fn mail_you_send_counts_its_recipients_bcc_too() {
        let s = store();
        put(&s, "Sent", 1, &raw(ME, "Ana <ana@x.com>, me@me.test", "Bo <bo@x.com>", "<1@x>", 1, "Bcc: Cy <cy@x.com>\r\n"));
        // Mail from you found elsewhere (an archive, All Mail) is yours too.
        put(&s, "Archive", 2, &raw(&format!("Me <{ME}>"), "Ana <ana@x.com>", "", "<2@x>", 1, ""));
        assert_eq!(row(&s, "ana@x.com"), (2, 0, 0, 0));
        assert_eq!(row(&s, "bo@x.com"), (1, 0, 0, 0));
        assert_eq!(row(&s, "cy@x.com"), (1, 0, 0, 0));
        let mine: i64 = s.conn.query_row("SELECT COUNT(*) FROM contacts WHERE email = ?1", [ME], |r| r.get(0)).unwrap();
        assert_eq!(mine, 0, "you aren't your own contact");
    }

    #[test]
    fn drafts_spam_and_bulk_recipients_count_for_nothing() {
        let s = store();
        put(&s, "Drafts", 1, &raw(ME, "Ana <ana@x.com>", "", "<1@x>", 1, ""));
        put(&s, "Junk", 1, &raw("Spammer <win@spam.example>", ME, "", "<2@x>", 1, ""));
        let p = mime::parse(raw(ME, "Bo <bo@x.com>", "", "<3@x>", 1, "").as_bytes(), true).unwrap();
        s.upsert_header("a", "Archive", 9, flags::DRAFT, 10, 0, &p).unwrap(); // a draft elsewhere (All Mail)
        put(&s, "INBOX", 2, &raw("List <list@lists.example>", "team@lists.example", "Cy <cy@x.com>", "<4@x>", 1, "List-Id: <team.lists.example>\r\n"));
        let all: Vec<String> = s.conn.prepare("SELECT email FROM contacts ORDER BY email").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(all, vec!["list@lists.example"], "only the list itself, as bulk");
        assert_eq!(row(&s, "list@lists.example"), (0, 0, 0, 1));
    }

    #[test]
    fn the_name_you_wrote_wins_and_its_words_are_found() {
        let s = store();
        put(&s, "INBOX", 1, &raw("ana@x.com", ME, "", "<1@x>", 5, ""));
        assert_eq!(s.suggest("ana", &me(), NOW, 8).unwrap()[0].name, "");
        put(&s, "INBOX", 2, &raw("Ana Lima <ana@x.com>", ME, "", "<2@x>", 4, ""));
        assert_eq!(emails(&s, "lima"), vec!["ana@x.com"], "found by the name it now has");
        put(&s, "Sent", 3, &raw(ME, "Mum <ana@x.com>", "", "<3@x>", 3, ""));
        put(&s, "INBOX", 4, &raw("Ana L. <ana@x.com>", ME, "", "<4@x>", 1, ""));
        assert_eq!(s.suggest("ana", &me(), NOW, 8).unwrap()[0].name, "Mum", "yours beats theirs, even newer");
        assert_eq!(emails(&s, "mum"), vec!["ana@x.com"]);
        assert!(emails(&s, "lima").is_empty(), "the old name's words are gone");
    }

    #[test]
    fn removing_an_account_removes_its_people() {
        let s = store();
        put(&s, "INBOX", 1, &raw("Ana <ana@x.com>", ME, "", "<1@x>", 1, ""));
        s.forget_account("a").unwrap();
        assert!(emails(&s, "ana").is_empty());
        let left: i64 = s.conn.query_row("SELECT (SELECT COUNT(*) FROM contacts) + (SELECT COUNT(*) FROM contact_terms) + (SELECT COUNT(*) FROM contact_sources)", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
    }

    /// A contact as stored: account, address, name, sent, sent_at, got, cc, bulk, seen_at.
    type Row = (String, String, String, u32, i64, u32, u32, u32, i64);

    fn dump(s: &Store) -> Vec<Row> {
        s.conn
            .prepare("SELECT account, email, name, sent, sent_at, got, cc, bulk, seen_at FROM contacts ORDER BY account, email")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Mail of every kind the counting tells apart, some of it downloaded and some not (not
    /// the kinds only headers tell: see [`a_message_an_old_cache_has_no_body_for_counts_as_its_row_says`]).
    fn mixed(s: &Store, from: u32, to: u32) {
        for i in from..to {
            let m = match i % 7 {
                0 => raw(&format!("Ana Lima <ana{}@x.com>", i % 3), ME, "Bo <bo@x.com>", &format!("<{i}@x>"), i as i64, ""),
                1 => raw(ME, &format!("Cy <cy{}@x.com>", i % 4), "", &format!("<{i}@x>"), i as i64, "Bcc: Dee <dee@x.com>\r\n"),
                2 => raw("News <news@n.example>", ME, "", &format!("<{i}@x>"), i as i64, "List-Unsubscribe: <https://n.example/u>\r\n"),
                3 => raw("Émile <emile@w.fr>", ME, "", "", i as i64, ""),
                4 => raw("Ana Lima <ana0@x.com>", ME, "", "<0@x>", 0, ""), // a copy of message 0
                5 => raw("Fay <fay@x.com>", ME, "Gus <gus@x.com>", &format!("<{i}@x>"), i as i64, &format!("X-Pad: {}\r\n", "p".repeat(40_000))),
                _ => raw(ME, "Hal <hal@x.com>", "", &format!("<{i}@x>"), i as i64, ""),
            };
            let mailbox = ["INBOX", "Sent", "INBOX", "Archive", "Label", "INBOX", "Sent"][(i % 7) as usize];
            let p = mime::parse(m.as_bytes(), false).unwrap();
            let id = s.upsert_header("a", mailbox, i, flags::SEEN, m.len() as u32, 0, &p).unwrap();
            if i % 5 != 0 || matches!(i % 7, 1 | 2) {
                s.set_body(id, m.as_bytes(), &p).unwrap();
            }
        }
    }

    #[test]
    fn an_old_cache_gets_its_people_counted_in_the_background_as_mail_keeps_coming() {
        let dir = std::env::temp_dir().join(format!("envoy-store-contacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        // What counting as mail comes gives, for all of it.
        let want = {
            let s = store();
            mixed(&s, 1, 61);
            dump(&s)
        };
        {
            // A cache from before contacts: the same first 40 messages.
            let s = Store::open(&db).unwrap();
            boxes(&s);
            mixed(&s, 1, 41);
            s.conn.execute_batch("DROP TABLE contacts; DROP TABLE contact_terms; DROP TABLE contact_sources;").unwrap();
        }
        let s = Store::open(&db).unwrap();
        s.set_me(me());
        assert!(dump(&s).is_empty() && emails(&s, "ana").is_empty(), "nothing counted yet");
        assert!(s.backfill_contacts(7).unwrap());
        // Mail arrives meanwhile, a copy of an old message among it.
        mixed(&s, 41, 51);
        drop(s);
        // A restart part way.
        let s = Store::open(&db).unwrap();
        s.set_me(me());
        let mut chunks = 0;
        while s.backfill_contacts(7).unwrap() {
            chunks += 1;
            assert!(chunks < 20, "the backfill ends");
        }
        mixed(&s, 51, 61);
        assert_eq!(dump(&s), want, "the same as counting it all as it came");
        assert!(!has_table(&s, "contacts_backfill"), "done with");
        assert!(!s.backfill_contacts(7).unwrap());
        drop(s);
        let s = Store::open(&db).unwrap();
        assert!(!s.backfill_contacts(7).unwrap(), "nothing left to do after a restart");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_message_an_old_cache_has_no_body_for_counts_as_its_row_says() {
        // Its row has its sender and recipients, but not whether it was bulk, nor its Bcc.
        let s = store();
        let news = raw("News <news@n.example>", ME, "", "<1@x>", 1, "List-Unsubscribe: <https://n.example/u>\r\n");
        let sent = raw(ME, "Ana <ana@x.com>", "", "<2@x>", 1, "Bcc: Bo <bo@x.com>\r\n");
        for (uid, (mailbox, m)) in [("INBOX", &news), ("Sent", &sent)].into_iter().enumerate() {
            s.upsert_header("a", mailbox, uid as u32 + 1, 0, 10, 0, &mime::parse(m.as_bytes(), true).unwrap()).unwrap();
        }
        s.conn.execute_batch("DELETE FROM contacts; DELETE FROM contact_sources; CREATE TABLE contacts_backfill (next INTEGER NOT NULL, upto INTEGER NOT NULL); INSERT INTO contacts_backfill VALUES (0, 2);").unwrap();
        s.contacts_backfill.set(true);
        while s.backfill_contacts(10).unwrap() {}
        assert_eq!(row(&s, "news@n.example"), (0, 1, 0, 0), "heard from, not bulk");
        assert_eq!(row(&s, "ana@x.com"), (1, 0, 0, 0));
        let bo: i64 = s.conn.query_row("SELECT COUNT(*) FROM contacts WHERE email = 'bo@x.com'", [], |r| r.get(0)).unwrap();
        assert_eq!(bo, 0, "its Bcc unknown");
    }

    #[test]
    fn headers_are_read_up_to_their_blank_line_wherever_it_falls() {
        let s = store();
        // A body starting with what looks like a list's header, and the blank line before it on
        // either side of where one read ends and the next begins.
        for pad in (PEEK_STEP - 110)..(PEEK_STEP - 80) {
            let head = format!("From: {ME}\r\nTo: Ana <ana@x.com>\r\nBcc: Bo <bo@x.com>\r\nMessage-ID: <{pad}@x>\r\nX-Pad: {}\r\n", "p".repeat(pad));
            let raw = format!("{head}\r\nList-Id: <not.a.header>\r\nbody\r\n");
            let p = mime::parse(raw.as_bytes(), false).unwrap();
            let id = s.upsert_header("a", "Sent", pad as u32, 0, 10, 0, &p).unwrap();
            s.set_body(id, raw.as_bytes(), &p).unwrap();
            let (bcc, bulk) = s.peek_headers(id).unwrap();
            assert_eq!((bcc.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), bulk), (vec!["bo@x.com"], false), "blank line at {}", head.len());
        }
    }

    fn has_table(s: &Store, name: &str) -> bool {
        super::super::has_table(&s.conn, name).unwrap()
    }

    #[test]
    fn a_new_cache_has_nothing_to_backfill() {
        let dir = std::env::temp_dir().join(format!("envoy-store-contacts-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("mail.sqlite")).unwrap();
        assert!(has_table(&s, "contacts") && !has_table(&s, "contacts_backfill"));
        assert!(!s.backfill_contacts(100).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lookup_reads_only_its_range_of_the_index() {
        let s = store();
        s.transaction(|st| {
            for i in 0..3_000u32 {
                let m = raw(&format!("Person{i} Surname{} <p{i}@x{}.com>", i % 97, i % 13), ME, "", &format!("<{i}@x>"), (i % 300) as i64, "");
                let p = mime::parse(m.as_bytes(), true).unwrap();
                st.upsert_header("a", "INBOX", i + 1, 0, 10, 0, &p)?;
            }
            Ok(())
        })
        .unwrap();
        for stats in ["without statistics", "after ANALYZE"] {
            if stats == "after ANALYZE" {
                s.conn.execute_batch("ANALYZE").unwrap();
            }
            let plan: Vec<String> =
                s.conn.prepare(&format!("EXPLAIN QUERY PLAN {SUGGEST}")).unwrap().query_map(params!["pe", "pe\u{10FFFF}"], |r| r.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert!(!plan.iter().any(|d| d.starts_with("SCAN")), "{stats}: {plan:?}");
            assert!(plan.iter().any(|d| d.starts_with("SEARCH contact_terms USING PRIMARY KEY (term>? AND term<?)")), "{stats}: {plan:?}");
            assert!(plan.iter().any(|d| d.starts_with("SEARCH contacts USING INTEGER PRIMARY KEY (rowid=?)")), "{stats}: {plan:?}");
        }
        let both: HashSet<String> = s.suggest("person12 surname12", &me(), NOW, 8).unwrap().into_iter().map(|a| a.email).collect();
        assert_eq!(both, ["p12@x12.com".to_owned(), "p1273@x12.com".to_owned()].into(), "both words must match");
        assert_eq!(s.suggest("surname1", &me(), NOW, 1000).unwrap().len(), 31 * 11, "Surname1 and Surname10..19 over 3000 people");
    }
}
