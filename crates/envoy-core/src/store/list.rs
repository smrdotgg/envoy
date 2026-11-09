//! [`Store::list`]: a view's rows, newest first, one per conversation in conversation view.
//!
//! Search runs on every keystroke, so this must stay fast on a broad query in a big folder
//! (`i`, `from:`, a common word in Archive). It never reads more than it shows:
//!
//! 1. Free text is looked up in the full-text index once, into a bitset of message ids.
//! 2. Rows stream newest first, by the covering index `messages_by_date` (one ordered scan per
//!    mailbox of the view, merged by SQLite), reading only what grouping needs, and stop at the
//!    first conversation past `limit`. A query whose words match only a few messages is driven
//!    from those instead (fetched by id, then sorted), so a rare word in a big folder costs what
//!    it matches.
//! 3. The conversations shown get the rest of their messages (older than where the scan
//!    stopped) by the thread index, for their counts, unread state and participants.
//! 4. Only the rows shown are read in full.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Row, params_from_iter};

use super::{Query, SUMMARY_COLS, Store, fts_term, summary_row};
use crate::model::{Address, Category, Role, Summary, View, flags};

/// The list's order, in every query that feeds it: newest first, and among messages of the
/// same second the newer id (stored later) first. Ids never change, so neither does the order
/// of a row when its flags do; and the scan, the few-matches path and the thread pass all meet
/// rows in this order, so which copy of a message stored twice counts (the first) can't
/// depend on which of them read it.
const ORDER: &str = "m.date DESC, m.id DESC";

/// What grouping reads, all in `messages_by_date`: the scan never touches the table.
const LIGHT_COLS: &str = "m.id, m.account, m.thread, m.message_id, m.date, m.flags, m.has_attachments, m.from_name, m.from_email";

/// A query whose words match at most this many messages is driven from its matches.
const FEW_HITS: usize = 4000;

/// Up to this many mailboxes are scanned one ordered index each, merged; more (SQLite takes at
/// most 500 parts of a compound query) go through one query, sorted.
const MERGED_BOXES: usize = 32;

/// Which messages a view covers.
pub(super) enum Scope {
    /// These mailboxes, `(account, name)`.
    Boxes(Vec<(String, String)>),
    /// Flagged messages anywhere but Trash and Spam, in one account or all.
    Starred(Option<String>),
}

/// The query's conditions on `messages m`, besides the view and its free text.
struct Filter {
    conds: Vec<String>,
    args: Vec<SqlValue>,
    /// The categories kept ([`Query::category`]), whose scan reads `messages_by_category`.
    categories: Option<Vec<i64>>,
}

impl Filter {
    fn new(query: &Query) -> Filter {
        let mut conds: Vec<String> = vec!["m.flags & 16 = 0".into(), HIDDEN_OUT.into()];
        let mut args: Vec<SqlValue> = Vec::new();
        let like = |v: &str| SqlValue::from(format!("%{v}%"));
        // LIKE ignores ASCII case and SQLite's lower() only lowers ASCII, so this matches what
        // `lower(column) LIKE` does without copying every value. An empty value (`from:` as
        // it's typed) matches every message: no condition at all.
        for f in query.from.iter().filter(|f| !f.is_empty()) {
            conds.push("(m.from_email LIKE ? OR m.from_name LIKE ?)".into());
            args.push(like(f));
            args.push(like(f));
        }
        for t in query.to.iter().filter(|t| !t.is_empty()) {
            conds.push("m.recipients LIKE ?".into());
            args.push(like(t));
        }
        for t in query.subject.iter().filter(|t| !t.is_empty()) {
            conds.push("m.subject LIKE ?".into());
            args.push(like(t));
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
        // Codes the code writes, not the user: inlined.
        let categories = query.categories().map(|mask| Category::ALL.iter().map(|c| c.code()).filter(|c| mask & 1 << c != 0).collect::<Vec<i64>>());
        if let Some(codes) = &categories {
            let list = if codes.is_empty() { "-1".to_owned() } else { codes.iter().map(i64::to_string).collect::<Vec<_>>().join(", ") };
            conds.push(format!("m.category IN ({list})"));
        }
        Filter { conds, args, categories }
    }

    fn sql(&self) -> String {
        self.conds.join(" AND ")
    }

    /// The conditions on a message on its way into the view ([`Arrival`]): the same, but for
    /// the one that leaves out hidden messages (a moved one is hidden where it's leaving).
    /// Takes the same arguments.
    fn sql_arriving(&self) -> String {
        self.conds.iter().filter(|c| c.as_str() != HIDDEN_OUT).cloned().collect::<Vec<_>>().join(" AND ")
    }
}

/// [`Filter`]'s condition leaving out messages hidden for a move.
const HIDDEN_OUT: &str = "m.held_op = 0";

/// The messages hidden for an op, with it ([`Store::arrivals`]): by `messages_held` (the `!= 0`
/// lets SQLite use it), a few rows.
const MOVING: &str = "SELECT m.id, m.account, o.op FROM messages m JOIN ops o ON o.id = m.held_op WHERE m.held_op != 0";
/// The ops held for an undo window, by `ops_batch`: never a send, whose op carries its message.
const HELD_OPS: &str = "SELECT account, op FROM ops WHERE batch != 0";

/// A message on its way into `dest`, one of `account`'s mailboxes: moved or copied there (a
/// label) by an op that hasn't reached the server yet, held for the undo window or waiting to
/// replay. A list of `dest` shows it at once ([`flags::ARRIVING`]); an Undo takes it back out,
/// and once the op has gone the server's copy, synced, takes its place.
pub(super) struct Arrival {
    pub id: i64,
    pub account: String,
    pub dest: String,
}

/// Message ids the free text matched.
struct Hits {
    ids: Vec<i64>,
    bits: Vec<u64>,
}

impl Hits {
    fn new(ids: Vec<i64>) -> Hits {
        let max = ids.iter().copied().max().unwrap_or(0).max(0) as usize;
        let mut bits = vec![0u64; max / 64 + 1];
        for &id in &ids {
            if id >= 0 {
                bits[id as usize / 64] |= 1 << (id as usize % 64);
            }
        }
        Hits { ids, bits }
    }

    fn contains(&self, id: i64) -> bool {
        id >= 0 && self.bits.get(id as usize / 64).is_some_and(|w| w & (1 << (id as usize % 64)) != 0)
    }
}

fn text<'a>(r: &'a Row<'_>, i: usize) -> rusqlite::Result<&'a str> {
    match r.get_ref(i)? {
        ValueRef::Text(t) => std::str::from_utf8(t).map_err(rusqlite::Error::Utf8Error),
        ValueRef::Null => Ok(""),
        other => Err(rusqlite::Error::InvalidColumnType(i, String::new(), other.data_type())),
    }
}

/// A list being read in pages: the whole list's limit, the first page's rows once handed out
/// (and how many of the grouper's they came from), and who they're handed to.
struct Pages<'a> {
    limit: usize,
    shown: Vec<Summary>,
    done: usize,
    first: &'a mut dyn FnMut(Vec<Summary>),
}

/// One conversation (or message) in the output, until it's read in full.
struct Row1 {
    id: i64,
    thread_count: u32,
    thread_unread: bool,
    has_attachments: bool,
    flagged: bool,
    names: Vec<String>,
    /// `account \0 thread`, in conversation view.
    tkey: String,
}

/// Groups rows as they stream in, newest first.
struct Grouper<'a> {
    conversations: bool,
    limit: usize,
    me: &'a HashSet<String>,
    hits: Option<&'a Hits>,
    out: Vec<Row1>,
    /// `account \0 thread` → index in `out`.
    threads: HashMap<String, usize>,
    /// `account message_id` of every message counted: the same message in two synced
    /// mailboxes (Gmail labels) counts once.
    seen: HashSet<String>,
    /// Ids already counted (the scan's), so the thread pass skips them.
    counted: HashSet<i64>,
    /// Scratch keys, so a row that's skipped or joins a conversation allocates nothing.
    key: String,
    tkey: String,
    /// The messages on their way into the view ([`Arrival`]), each with where it's going.
    arriving: &'a HashMap<i64, String>,
}

enum Took {
    Counted,
    Skipped,
    /// The first message of a conversation that doesn't fit: the scan can stop.
    Full,
}

impl Grouper<'_> {
    fn take(&mut self, r: &Row, grow: bool) -> rusqlite::Result<Took> {
        let id: i64 = r.get(0)?;
        if self.hits.is_some_and(|h| !h.contains(id)) || self.counted.contains(&id) {
            return Ok(Took::Skipped);
        }
        let account = text(r, 1)?;
        let message_id = text(r, 3)?;
        if !message_id.is_empty() {
            self.key.clear();
            self.key.push_str(account);
            self.key.push_str(message_id);
            if self.seen.contains(self.key.as_str()) {
                return Ok(Took::Skipped);
            }
        }
        let fl: u32 = r.get(5)?;
        let attach = r.get::<_, i64>(6)? != 0;
        let unseen = fl & flags::SEEN == 0;
        if self.conversations {
            self.tkey.clear();
            self.tkey.push_str(account);
            self.tkey.push('\0');
            self.tkey.push_str(text(r, 2)?);
            if let Some(&i) = self.threads.get(self.tkey.as_str()) {
                let name = super::first_name(&Address { name: text(r, 7)?.to_owned(), email: text(r, 8)?.to_owned() }, self.me);
                let row = &mut self.out[i];
                row.thread_count += 1;
                row.thread_unread |= unseen;
                row.has_attachments |= attach;
                row.flagged |= fl & flags::FLAGGED != 0;
                if !row.names.contains(&name) {
                    row.names.push(name);
                }
            } else {
                if !grow {
                    return Ok(Took::Skipped);
                }
                if self.out.len() >= self.limit {
                    return Ok(Took::Full);
                }
                let name = super::first_name(&Address { name: text(r, 7)?.to_owned(), email: text(r, 8)?.to_owned() }, self.me);
                self.threads.insert(self.tkey.clone(), self.out.len());
                self.out.push(Row1 { id, thread_count: 1, thread_unread: unseen, has_attachments: attach, flagged: fl & flags::FLAGGED != 0, names: vec![name], tkey: self.tkey.clone() });
            }
        } else {
            if self.out.len() >= self.limit {
                return Ok(Took::Full);
            }
            self.out.push(Row1 { id, thread_count: 1, thread_unread: unseen, has_attachments: attach, flagged: fl & flags::FLAGGED != 0, names: Vec::new(), tkey: String::new() });
        }
        if !message_id.is_empty() {
            self.seen.insert(std::mem::take(&mut self.key));
        }
        self.counted.insert(id);
        Ok(Took::Counted)
    }
}

impl Store {
    pub(super) fn scope(&self, view: &View) -> Result<Scope> {
        Ok(match view {
            View::Mailbox { account, mailbox } => Scope::Boxes(vec![(account.clone(), mailbox.clone())]),
            // Every bucket not done with (and the server's own snoozed mailbox), soonest to
            // wake first: the order Snoozed lists them in. Only names that are buckets have the
            // role with a stamp at their end, and stamps sort as their times do.
            View::Role { account, role: Role::Snoozed } => {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT account, name FROM mailboxes m WHERE role = 'snoozed' AND selectable = 1 AND (?1 IS NULL OR account = ?1)
                       AND NOT EXISTS (SELECT 1 FROM snooze_done d WHERE d.account = m.account AND d.name = m.name)
                     ORDER BY substr(name, -14) GLOB '[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9]Z' DESC, substr(name, -14), account, name",
                )?;
                Scope::Boxes(stmt.query_map([account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
            }
            View::Role { account, role } => {
                let mut stmt = self.conn.prepare_cached("SELECT account, name FROM mailboxes WHERE role = ?1 AND (?2 IS NULL OR account = ?2) ORDER BY account, name")?;
                let boxes = stmt.query_map(rusqlite::params![role.as_str(), account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                Scope::Boxes(boxes)
            }
            View::Starred { account } => Scope::Starred(account.clone()),
        })
    }

    /// `account`'s mailboxes that `view` shows now (Snoozed: its buckets not done with, soonest
    /// first): what a worker keeps synced while the view's on screen, read again each round.
    pub fn watched_boxes(&self, view: &View, account: &str) -> Result<Vec<String>> {
        Ok(match self.scope(view)? {
            Scope::Boxes(boxes) => boxes.into_iter().filter(|(a, _)| a == account).map(|(_, m)| m).collect(),
            Scope::Starred(_) => Vec::new(),
        })
    }

    /// Every message on its way somewhere ([`Arrival`]). A move hides its message where it's
    /// leaving ([`Store::hide_local`]), so moves are found by the hidden messages' index; a copy
    /// leaves its message where it is, and is found among the ops held for an undo window
    /// (moves, copies and discards: small ones; a send's carries its whole message, and isn't
    /// read). A copy queued with Undo off goes at once when online, and shows once it's synced.
    pub(super) fn arrivals(&self) -> Result<Vec<Arrival>> {
        let mut out = Vec::new();
        let mut moves = self.conn.prepare_cached(MOVING)?;
        let rows = moves.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for row in rows {
            let (id, account, op) = row?;
            if let Ok(super::Op::Move { dest, .. }) = serde_json::from_str(&op) {
                out.push(Arrival { id, account, dest });
            }
        }
        let held: Vec<(String, String)> =
            self.conn.prepare_cached(HELD_OPS)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (account, op) in held {
            if let Ok(super::Op::Copy { mailbox, uid, dest }) = serde_json::from_str(&op)
                && let Some(id) = self.id_for_uid(&account, &mailbox, uid)?
                && !out.iter().any(|a: &Arrival| a.id == id && a.dest == dest)
            {
                out.push(Arrival { id, account, dest });
            }
        }
        Ok(out)
    }

    /// The messages on their way into `boxes`, `(account, name)`s, each with where it's going.
    pub(super) fn arriving_in(&self, boxes: &[(String, String)]) -> Result<HashMap<i64, String>> {
        Ok(self.arrivals()?.into_iter().filter(|a| boxes.iter().any(|(acc, b)| *acc == a.account && *b == a.dest)).map(|a| (a.id, a.dest)).collect())
    }

    fn fts_hits(&self, query: &Query) -> Result<Hits> {
        let m: Vec<String> = query.text.iter().map(|t| fts_term(t)).collect();
        let mut stmt = self.conn.prepare_cached("SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY rowid")?;
        let ids = stmt.query_map([m.join(" AND ")], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(Hits::new(ids))
    }

    /// See the module docs. With `0 < page < limit`, `first` gets the first `page` rows as soon
    /// as they're known, counts and all (they're final), and the rest is read after.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn list_rows(
        &self,
        view: &View,
        query: &Query,
        conversations: bool,
        limit: u32,
        me: &HashSet<String>,
        page: u32,
        first: &mut dyn FnMut(Vec<Summary>),
    ) -> Result<Vec<Summary>> {
        let scope = self.scope(view)?;
        if limit == 0 || matches!(&scope, Scope::Boxes(b) if b.is_empty()) {
            return Ok(Vec::new());
        }
        let filter = Filter::new(query);
        let hits = if query.text.is_empty() { None } else { Some(self.fts_hits(query)?) };
        if hits.as_ref().is_some_and(|h| h.ids.is_empty()) {
            return Ok(Vec::new());
        }
        // Snoozed goes by wake time, not date, a message a row: each bucket in the scope's order.
        let snoozed = matches!(view, View::Role { role: Role::Snoozed, .. });
        let conversations = conversations && !snoozed;
        // Messages back from a snooze come first in the inbox, and in their own tab of the split
        // inbox (the filter keeps its category), until they're dealt with; a search goes by date.
        let pinned = if query.is_search() { Vec::new() } else { self.pinned_boxes(view, &scope) };
        // Moved or labeled here, and not on the server yet: here already, by date.
        let arriving = match &scope {
            Scope::Boxes(boxes) => self.arriving_in(boxes)?,
            Scope::Starred(_) => HashMap::new(),
        };
        let (limit, page) = (limit as usize, page as usize);
        let mut g = Grouper {
            conversations,
            limit: if page > 0 && page < limit { page } else { limit },
            me,
            hits: hits.as_ref(),
            out: Vec::new(),
            threads: HashMap::new(),
            seen: HashSet::new(),
            counted: HashSet::new(),
            key: String::new(),
            tkey: String::new(),
            arriving: &arriving,
        };

        // The scan, newest first.
        let few = hits.as_ref().filter(|h| h.ids.len() <= FEW_HITS);
        let arriving_ids: Vec<i64> = arriving.keys().copied().collect();
        let (sql, args) = scan_sql(&scope, &filter, few, (!arriving_ids.is_empty()).then_some(arriving_ids.as_slice()));
        // One snapshot for the scan, the thread pass and the full reads, while sync writes.
        let _read = self.conn.unchecked_transaction()?;
        let mut pages = Pages { limit, shown: Vec::new(), done: 0, first };
        // Every pinned row before any other: they're the list's top, first page or not.
        let mut passes = Vec::new();
        if !pinned.is_empty() {
            passes.push(pinned_sql(&pinned, &filter));
        }
        passes.push((sql, args));
        let mut stopped = false;
        if let (true, Scope::Boxes(boxes)) = (snoozed, &scope) {
            // One statement, run for each bucket in turn: its shape doesn't grow with them.
            let mut stmt = self.conn.prepare(&box_sql(&filter))?;
            'boxes: for (a, b) in boxes {
                let args: Vec<SqlValue> = [SqlValue::from(a.clone()), SqlValue::from(b.clone())].into_iter().chain(filter.args.iter().cloned()).collect();
                let mut rows = stmt.query(params_from_iter(args.iter()))?;
                while let Some(r) = rows.next()? {
                    if self.take_paged(&mut g, r, &mut pages, &scope, &filter)? {
                        stopped = true;
                        break 'boxes;
                    }
                }
            }
            passes.clear();
        }
        'passes: for (sql, args) in passes {
            let mut stmt = self.conn.prepare(&sql)?;
            let mut rows = stmt.query(params_from_iter(args.iter()))?;
            while let Some(r) = rows.next()? {
                if self.take_paged(&mut g, r, &mut pages, &scope, &filter)? {
                    stopped = true;
                    break 'passes;
                }
            }
        }
        // The conversations shown, completed with their older messages: only needed when the
        // scan stopped early (otherwise it read every match).
        let Pages { mut shown, done, .. } = pages;
        if conversations && stopped {
            self.complete(&mut g, done, &scope, &filter)?;
        }
        let rest = self.read_full(&g.out[done..], g.arriving)?;
        shown.extend(rest);
        Ok(shown)
    }

    /// Takes row `r` into the list: once the first page fills, it's completed and handed out,
    /// and reading goes on to the whole list's limit. True when the whole list is full. (One
    /// body for the loops that read rows, not a copy in each.)
    #[inline(never)]
    fn take_paged(&self, g: &mut Grouper, r: &Row, pages: &mut Pages, scope: &Scope, filter: &Filter) -> Result<bool> {
        let mut took = g.take(r, true)?;
        if matches!(took, Took::Full) && g.limit < pages.limit {
            self.complete(g, 0, scope, filter)?;
            pages.shown = self.read_full(&g.out, g.arriving)?;
            pages.done = g.out.len();
            (pages.first)(pages.shown.clone());
            g.limit = pages.limit;
            took = g.take(r, true)?;
        }
        Ok(matches!(took, Took::Full))
    }

    /// The mailboxes of `view` whose woken messages ([`flags::WOKEN`]) come first: the inbox's.
    fn pinned_boxes(&self, view: &View, scope: &Scope) -> Vec<(String, String)> {
        let Scope::Boxes(boxes) = scope else { return Vec::new() };
        match view {
            View::Role { role: Role::Inbox, .. } => boxes.clone(),
            View::Mailbox { account, mailbox } if self.role_of(account, mailbox) == Role::Inbox => boxes.clone(),
            _ => Vec::new(),
        }
    }

    /// Counts the older messages of `g.out[from..]`'s conversations: messages the scan didn't
    /// reach, found by the thread index.
    fn complete(&self, g: &mut Grouper, from: usize, scope: &Scope, filter: &Filter) -> Result<()> {
        if !g.conversations {
            return Ok(());
        }
        let mut by_account: HashMap<String, Vec<String>> = HashMap::new();
        for row in &g.out[from..] {
            if let Some((a, t)) = row.tkey.split_once('\0') {
                by_account.entry(a.to_owned()).or_default().push(t.to_owned());
            }
        }
        let mut accounts: Vec<_> = by_account.into_iter().collect();
        accounts.sort();
        for (account, threads) in accounts {
            let mut args: Vec<SqlValue> = vec![account.clone().into(), SqlValue::from(serde_json::to_string(&threads).unwrap_or_else(|_| "[]".into()))];
            args.extend(filter.args.iter().cloned());
            let within = match scope {
                Scope::Boxes(boxes) => {
                    let names: Vec<&str> = boxes.iter().filter(|(a, _)| *a == account).map(|(_, b)| b.as_str()).collect();
                    args.push(SqlValue::from(serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())));
                    "m.mailbox IN (SELECT value FROM json_each(?))".to_owned()
                }
                Scope::Starred(_) => scope_sql(scope, &mut args),
            };
            let sql = format!(
                "SELECT {LIGHT_COLS} FROM messages m INDEXED BY messages_thread WHERE m.account = ? AND m.thread IN (SELECT value FROM json_each(?)) AND {} AND {within} ORDER BY {ORDER}",
                filter.sql()
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut rows = stmt.query(params_from_iter(args.iter()))?;
            while let Some(r) = rows.next()? {
                g.take(r, false)?;
            }
        }
        Ok(())
    }

    /// `rows` read in full, with what grouping found. A message on its way into the view is
    /// shown as it will be there: in the mailbox it's going to, marked [`flags::ARRIVING`].
    fn read_full(&self, rows: &[Row1], arriving: &HashMap<i64, String>) -> Result<Vec<Summary>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let mut full: HashMap<i64, Summary> = HashMap::with_capacity(ids.len());
        {
            let mut stmt = self.conn.prepare_cached(&format!("SELECT {SUMMARY_COLS} FROM messages m WHERE m.id IN (SELECT value FROM json_each(?1))"))?;
            let mut q = stmt.query([ids_json(&ids)])?;
            while let Some(r) = q.next()? {
                let (s, _) = summary_row(r)?;
                full.insert(s.id, s);
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(mut s) = full.remove(&row.id) else { continue };
            s.thread_count = row.thread_count;
            s.thread_unread = row.thread_unread;
            s.has_attachments |= row.has_attachments;
            if row.flagged {
                s.flags |= flags::FLAGGED;
            }
            if row.thread_count > 1 {
                // Oldest participant first, as Gmail shows them.
                s.participants = row.names.iter().rev().cloned().collect::<Vec<_>>().join(", ");
            }
            if let Some(dest) = arriving.get(&s.id) {
                s.mailbox.clone_from(dest);
                s.flags |= flags::ARRIVING;
            }
            out.push(s);
        }
        Ok(out)
    }
}

/// The scan's SQL and its arguments: a view's matches in [`ORDER`], with the messages
/// `arriving` ([`Arrival`], read by id: a few) among them, when there are any.
fn scan_sql(scope: &Scope, filter: &Filter, few: Option<&Hits>, arriving: Option<&[i64]>) -> (String, Vec<SqlValue>) {
    let mut args: Vec<SqlValue> = Vec::new();
    // Each a SELECT of LIGHT_COLS in ORDER (or sorted into it).
    let mut arms: Vec<String> = Vec::new();
    match (scope, few) {
        (_, Some(h)) => {
            // Few matches: fetch them by id and sort; no need to scan the folder.
            args.push(SqlValue::from(ids_json(&h.ids)));
            args.extend(filter.args.iter().cloned());
            let within = scope_sql(scope, &mut args);
            arms.push(format!("SELECT {LIGHT_COLS} FROM messages m WHERE m.id IN (SELECT value FROM json_each(?)) AND {} AND {within}", filter.sql()));
        }
        (Scope::Boxes(boxes), None) if boxes.len() > MERGED_BOXES => {
            // Many mailboxes: one query whatever their number (its rows sorted).
            args.extend(filter.args.iter().cloned());
            let within = scope_sql(scope, &mut args);
            arms.push(format!("SELECT {LIGHT_COLS} FROM messages m WHERE {} AND {within}", filter.sql()));
        }
        (Scope::Boxes(boxes), None) => {
            // One ordered index scan per mailbox; SQLite merges them by date. With categories,
            // one per mailbox and category, each a range of `messages_by_category` in the same
            // order: a category of the inbox reads like a folder of its own.
            for (a, b) in boxes {
                let mut arm = |category: Option<i64>| {
                    args.push(a.clone().into());
                    args.push(b.clone().into());
                    if let Some(c) = category {
                        args.push(c.into());
                    }
                    args.extend(filter.args.iter().cloned());
                    let (index, category) = if category.is_some() { ("messages_by_category", " AND m.category = ?") } else { ("messages_by_date", "") };
                    arms.push(format!("SELECT {LIGHT_COLS} FROM messages m INDEXED BY {index} WHERE m.account = ? AND m.mailbox = ?{category} AND {}", filter.sql()));
                };
                match &filter.categories {
                    Some(codes) => codes.iter().for_each(|c| arm(Some(*c))),
                    None => arm(None),
                }
            }
            if arms.is_empty() {
                // Categories that name none: nothing matches.
                args.extend(filter.args.iter().cloned());
                return (format!("SELECT {LIGHT_COLS} FROM messages m WHERE 0 AND {}", filter.sql()), args);
            }
        }
        (Scope::Starred(_), None) => {
            args.extend(filter.args.iter().cloned());
            let within = scope_sql(scope, &mut args);
            arms.push(format!("SELECT {LIGHT_COLS} FROM messages m WHERE {} AND {within}", filter.sql()));
        }
    }
    if let Some(ids) = arriving {
        args.push(SqlValue::from(ids_json(ids)));
        args.extend(filter.args.iter().cloned());
        arms.push(format!("SELECT {LIGHT_COLS} FROM messages m WHERE m.id IN (SELECT value FROM json_each(?)) AND {}", filter.sql_arriving()));
    }
    // Columns 5 and 1 of LIGHT_COLS: date, id.
    let sql = if arms.len() == 1 { format!("{} ORDER BY {ORDER}", arms[0]) } else { format!("{} ORDER BY 5 DESC, 1 DESC", arms.join(" UNION ALL ")) };
    (sql, args)
}

/// The woken messages of `boxes`, newest first, by `messages_woken` (a handful of rows).
fn pinned_sql(boxes: &[(String, String)], filter: &Filter) -> (String, Vec<SqlValue>) {
    let mut args: Vec<SqlValue> = Vec::new();
    let arms: Vec<String> = boxes
        .iter()
        .map(|(a, b)| {
            args.push(a.clone().into());
            args.push(b.clone().into());
            args.extend(filter.args.iter().cloned());
            // The planner, without statistics, would read the inbox's whole date index instead.
            format!("SELECT {LIGHT_COLS} FROM messages m INDEXED BY messages_woken WHERE m.account = ? AND m.mailbox = ? AND m.flags & {} != 0 AND {}", flags::WOKEN, filter.sql())
        })
        .collect();
    (format!("{} ORDER BY 5 DESC, 1 DESC", arms.join(" UNION ALL ")), args)
}

/// `[1,2,3]`, for `json_each`.
fn ids_json(ids: &[i64]) -> String {
    let mut s = String::with_capacity(ids.len() * 7 + 2);
    s.push('[');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&id.to_string());
    }
    s.push(']');
    s
}

/// A Snoozed bucket's rows, newest first: run for each bucket, `(account, mailbox)` then the
/// filter's arguments.
fn box_sql(filter: &Filter) -> String {
    format!("SELECT {LIGHT_COLS} FROM messages m INDEXED BY messages_by_date WHERE m.account = ? AND m.mailbox = ? AND {} ORDER BY {ORDER}", filter.sql())
}

/// The scope as a condition on `m`, its arguments appended to `args`: one, however many
/// mailboxes.
fn scope_sql(scope: &Scope, args: &mut Vec<SqlValue>) -> String {
    match scope {
        Scope::Boxes(boxes) => {
            args.push(SqlValue::from(serde_json::to_string(boxes).unwrap_or_else(|_| "[]".into())));
            "(m.account, m.mailbox) IN (SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]') FROM json_each(?))".to_owned()
        }
        Scope::Starred(account) => {
            let mut s = format!(
                "m.flags & 2 != 0 AND (m.account, m.mailbox) NOT IN (SELECT account, name FROM mailboxes WHERE role IN ('{}', '{}'))",
                Role::Trash.as_str(),
                Role::Junk.as_str()
            );
            if let Some(a) = account {
                s.push_str(" AND m.account = ?");
                args.push(a.clone().into());
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan reads the index in the list's order, one mailbox or several merged: no sort, so
    /// it can stop at the limit.
    #[test]
    fn the_scan_reads_its_index_in_order() {
        let st = Store::memory();
        let filter = Filter::new(&Query::default());
        for boxes in [vec![("a".to_owned(), "INBOX".to_owned())], vec![("a".to_owned(), "INBOX".to_owned()), ("b".to_owned(), "INBOX".to_owned())]] {
            let n = boxes.len();
            let (sql, args) = scan_sql(&Scope::Boxes(boxes), &filter, None, None);
            let mut stmt = st.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = stmt.query_map(params_from_iter(args.iter()), |r| r.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert!(plan.iter().any(|p| p.contains("COVERING INDEX messages_by_date")), "{n} mailboxes: {plan:?}");
            assert!(!plan.iter().any(|p| p.contains("TEMP B-TREE")), "{n} mailboxes: {plan:?}");
        }
    }

    fn plan(st: &Store, sql: &str, args: &[SqlValue]) -> Vec<String> {
        let mut stmt = st.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(params_from_iter(args.iter()), |r| r.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    /// A tab of the split inbox (one category, in one inbox or several) reads its range of
    /// `messages_by_category` in the list's order, as a folder reads `messages_by_date`: no table,
    /// no sort, so it stops at the limit and costs what today's list costs. So does `category:n`
    /// (two categories). The tabs' counts read only unread messages; a sender's mail is found by
    /// its index.
    #[test]
    fn a_category_reads_its_own_index_in_order() {
        let st = Store::memory();
        for (boxes, q) in [
            (vec![("a".to_owned(), "INBOX".to_owned())], "category:important"),
            (vec![("a".to_owned(), "INBOX".to_owned()), ("b".to_owned(), "INBOX".to_owned())], "category:newsletters"),
            (vec![("a".to_owned(), "INBOX".to_owned())], "category:n is:unread"),
        ] {
            let filter = Filter::new(&Query::parse(q));
            let (sql, args) = scan_sql(&Scope::Boxes(boxes.clone()), &filter, None, None);
            let plan = plan(&st, &sql, &args);
            assert!(plan.iter().any(|p| p.contains("COVERING INDEX messages_by_category")), "{q}: {plan:?}");
            assert!(!plan.iter().any(|p| p.contains("TEMP B-TREE") || p.contains("messages_by_date")), "{q}: {plan:?}");
        }
        let counts = plan(&st, super::super::categories::COUNT_UNREAD, &["a".to_owned().into(), "INBOX".to_owned().into()]);
        assert!(counts.iter().any(|p| p.contains("COVERING INDEX messages_unread")), "{counts:?}");
        let senders = plan(&st, super::super::categories::SENDERS_THREADS, &[r#"["a@x.example"]"#.to_owned().into()]);
        assert!(senders.iter().any(|p| p.contains("INDEX messages_from")), "{senders:?}");
        let thread = plan(&st, super::super::categories::THREAD_ROWS, &["a".to_owned().into(), "t".to_owned().into()]);
        assert!(thread.iter().any(|p| p.contains("INDEX messages_thread")), "{thread:?}");
        // Setting a conversation's category reads only it (left to itself SQLite walks the
        // account in messages_by_category: quadratic over a download).
        let set = plan(&st, super::super::categories::SET_CATEGORY, &["a".to_owned().into(), "t".to_owned().into(), 1i64.into()]);
        assert!(set.iter().any(|p| p.contains("INDEX messages_thread (account=? AND thread=?)")), "{set:?}");
    }

    /// A Snoozed bucket is read by its date index; many mailboxes are one query of one scope
    /// parameter, not a part (or a term) each.
    #[test]
    fn query_shapes_dont_grow_with_the_mailboxes() {
        let st = Store::memory();
        let filter = Filter::new(&Query::default());
        let one = plan(&st, &box_sql(&filter), &[SqlValue::from("a".to_owned()), SqlValue::from("Snoozed/20261110T0800Z".to_owned())]);
        assert!(one.iter().any(|p| p.contains("COVERING INDEX messages_by_date (account=? AND mailbox=?)")) && !one.iter().any(|p| p.contains("TEMP B-TREE")), "{one:?}");
        let boxes: Vec<(String, String)> = (0..600).map(|i| ("a".to_owned(), format!("Snoozed/{i}"))).collect();
        let scope = Scope::Boxes(boxes);
        let (sql, args) = scan_sql(&scope, &filter, None, None);
        assert_eq!(args.len(), 1, "one scope parameter");
        assert!(!sql.contains("UNION"), "{sql}");
        assert!(!plan(&st, &sql, &args).is_empty());
        let hits = Hits::new((1..100).collect());
        let (sql, args) = scan_sql(&scope, &filter, Some(&hits), None);
        assert_eq!(args.len(), 2);
        assert!(!plan(&st, &sql, &args).is_empty());
    }

    /// What's on its way into a view is found by the hidden messages' index and the held ops',
    /// on every list: never by reading the cache, or the whole queue (a send's op carries its
    /// whole message).
    #[test]
    fn arrivals_are_found_by_their_indexes() {
        let st = Store::memory();
        let moving = plan(&st, MOVING, &[]);
        assert!(moving.iter().any(|p| p.contains("INDEX messages_held")) && !moving.iter().any(|p| p.starts_with("SCAN m") && !p.contains("INDEX")), "{moving:?}");
        let held = plan(&st, HELD_OPS, &[]);
        assert!(held.iter().any(|p| p.contains("INDEX ops_batch")), "{held:?}");
    }

    /// The inbox's woken messages are found by their own small index, not by reading the inbox:
    /// in a tab of the split inbox too.
    #[test]
    fn pinned_rows_are_read_by_their_own_index() {
        let st = Store::memory();
        for query in [Query::default(), Query::in_category(Category::Newsletters)] {
            let (sql, args) = pinned_sql(&[("a".to_owned(), "INBOX".to_owned()), ("b".to_owned(), "INBOX".to_owned())], &Filter::new(&query));
            let plan = plan(&st, &sql, &args);
            assert!(plan.iter().filter(|p| p.contains("INDEX messages_woken")).count() == 2, "{query:?}: {plan:?}");
        }
    }
}
