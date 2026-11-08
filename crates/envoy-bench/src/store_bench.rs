//! Search and open, timed on the engine's own code path: the same `Store` calls, arguments and
//! connection setup the engine thread makes for `Command::List` and `Command::Open`.
//!
//! - Search: `Query::parse` + `Store::list(view, query, conversations = true, limit = 3000, me)`,
//!   exactly what one keystroke in the search field costs (search runs as you type). Each query
//!   runs once cold (first time in this process) and then `REPEAT` more times.
//! - Open: `Store::thread(id, conversations = true)`: reads the conversation, its bodies,
//!   converts HTML to Markdown on first open (and writes it back, as the engine does), and
//!   parses the HTML into the layout tree the reader draws. Each sample is a different message,
//!   so "first open" is really first.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use envoy_core::model::{Role, View};
use envoy_core::store::{Query, Store};

use crate::mailbox::{ACCOUNT, ME};
use crate::report::{Check, budget, p50_max};

const REPEAT: usize = 10;
const LIMIT: u32 = 3000; // what `App::request_list` asks for

fn inbox() -> View {
    View::Role { account: None, role: Role::Inbox }
}

fn archive() -> View {
    View::Role { account: None, role: Role::Archive }
}

/// What people type, finished: a common word (in ~25% of mail), a medium one (a few percent), a rare
/// one, two words, the filters, a miss. Inbox is the default view; Archive is the big folder.
const QUERIES: &[(&str, &str)] = &[
    ("inbox", "meeting"),
    ("inbox", "invoice"),
    ("inbox", "budget review"),
    ("inbox", "pancakes"),
    ("inbox", "from:alice"),
    ("inbox", "from:northwind"),
    ("inbox", "to:maya"),
    ("inbox", "subject:lunch"),
    ("inbox", "is:unread"),
    ("inbox", "is:starred"),
    ("inbox", "has:attachment"),
    ("inbox", "from:sam is:unread"),
    ("inbox", "qwxzv"),
    ("archive", "meeting"),
    ("archive", "invoice"),
    ("archive", "from:alice"),
    ("archive", "is:unread"),
];

/// Search runs on every keystroke, so the prefixes on the way to a query are searches too.
const TYPED: &[(&str, &str)] = &[("inbox", "invoice"), ("inbox", "from:alice")];

fn view_of(name: &str) -> View {
    if name == "archive" { archive() } else { inbox() }
}

fn time_query(st: &Store, view: &View, q: &str, me: &HashSet<String>) -> Result<(f64, usize)> {
    let t = Instant::now();
    let query = Query::parse(q);
    let rows = st.list(view, &query, true, LIMIT, me)?;
    Ok((t.elapsed().as_secs_f64() * 1000.0, rows.len()))
}

pub fn run(home: &Path, checks: &mut Vec<Check>, search: bool, open: bool) -> Result<()> {
    let db = home.join("mail.sqlite");
    anyhow::ensure!(db.exists(), "no mailbox at {} (run `envoy-bench gen` first)", db.display());
    let me: HashSet<String> = [ME.to_string()].into();

    // Startup's data path, for the breakdown: open the cache and read the first screen.
    let t = Instant::now();
    let st = Store::open(&db)?;
    let opened = t.elapsed().as_secs_f64() * 1000.0;
    let (first_list, rows) = time_query(&st, &inbox(), "", &me)?;
    let total: i64 = rusqlite::Connection::open(&db)?.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    checks.push(Check::info("store", "open cache", opened, "ms", format!("{total} messages in {}", human_size(std::fs::metadata(&db)?.len()))));
    checks.push(Check::info("store", "first inbox list", first_list, "ms", format!("{rows} conversations (startup's query, first run in this process)")));

    if search {
        run_search(&st, &me, checks)?;
    }
    if open {
        run_open(&db, &st, checks)?;
    }
    Ok(())
}

fn run_search(st: &Store, me: &HashSet<String>, checks: &mut Vec<Check>) -> Result<()> {
    let mut all = Vec::new();
    let mut lines = Vec::new();
    for (v, q) in QUERIES {
        let view = view_of(v);
        let (cold, n) = time_query(st, &view, q, me)?;
        let mut runs = vec![cold];
        for _ in 0..REPEAT {
            runs.push(time_query(st, &view, q, me)?.0);
        }
        let (p50, max) = p50_max(&runs);
        lines.push(format!("  {v:>7}  {q:<22} {n:>5} rows  cold {cold:7.2} ms  p50 {p50:7.2} ms  max {max:7.2} ms"));
        all.extend(runs);
        checks.push(Check::gated("search", format!("{v}: {q}"), max, "ms", budget::SEARCH_MS, 1.0, format!("worst of {}; p50 {p50:.2} ms, {n} rows", REPEAT + 1)));
    }
    let mut typed_all = Vec::new();
    for (v, q) in TYPED {
        let view = view_of(v);
        let mut worst = (0.0f64, String::new());
        for end in 1..=q.chars().count() {
            let prefix: String = q.chars().take(end).collect();
            let (cold, _) = time_query(st, &view, &prefix, me)?;
            let (warm, n) = time_query(st, &view, &prefix, me)?;
            let ms = cold.max(warm);
            typed_all.push(ms);
            lines.push(format!("  {v:>7}  {:<22} {n:>5} rows  cold {cold:7.2} ms  warm {warm:7.2} ms   (typing)", format!("{prefix:?}")));
            if ms > worst.0 {
                worst = (ms, prefix);
            }
        }
        checks.push(Check::gated("search", format!("{v}: typing {q:?}"), worst.0, "ms", budget::SEARCH_MS, 1.0, format!("worst keystroke {:?} of {}", worst.1, q.chars().count())));
    }
    println!("\nsearch (Store::list, conversations, limit {LIMIT}):");
    for l in &lines {
        println!("{l}");
    }
    all.extend(typed_all);
    let (p50, max) = p50_max(&all);
    checks.push(Check::gated("search", "all queries", max, "ms", budget::SEARCH_MS, 1.0, format!("worst of {} runs; p50 {p50:.2} ms", all.len())));
    Ok(())
}

fn run_open(db: &Path, st: &Store, checks: &mut Vec<Check>) -> Result<()> {
    let pick = rusqlite::Connection::open(db)?;
    let ids = |sql: &str| -> Result<Vec<i64>> {
        let mut stmt = pick.prepare(sql)?;
        let rows = stmt.query_map([ACCOUNT], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    let plain = ids(
        "SELECT m.id FROM messages m JOIN bodies b ON b.id = m.id WHERE m.account = ?1 AND m.mailbox = 'INBOX' AND b.html IS NULL
         AND (SELECT COUNT(*) FROM messages t WHERE t.account = m.account AND t.thread = m.thread) = 1 ORDER BY m.date DESC LIMIT 40",
    )?;
    let html = ids(
        "SELECT m.id FROM messages m JOIN bodies b ON b.id = m.id WHERE m.account = ?1 AND b.html IS NOT NULL AND b.markdown IS NULL
         AND length(b.html) > 8000 ORDER BY m.date DESC LIMIT 40",
    )?;
    let threads = ids(
        "SELECT MAX(m.id) FROM messages m WHERE m.account = ?1 GROUP BY m.thread HAVING COUNT(*) >= 6 ORDER BY MAX(m.date) DESC LIMIT 30",
    )?;
    let mut time_open = |name: &str, ids: &[i64], what: &str| -> Result<()> {
        anyhow::ensure!(!ids.is_empty(), "no {name} messages in the mailbox");
        let mut runs = Vec::new();
        let mut msgs = 0;
        for &id in ids {
            let t = Instant::now();
            let thread = st.thread(id, true)?;
            runs.push(t.elapsed().as_secs_f64() * 1000.0);
            anyhow::ensure!(thread.iter().all(|m| m.body.is_some()), "message {id} has no cached body");
            msgs += thread.len();
        }
        let (p50, max) = p50_max(&runs);
        println!("  open {name:<18} {:>3} samples  p50 {p50:7.2} ms  max {max:7.2} ms  ({:.1} messages each)", runs.len(), msgs as f64 / runs.len() as f64);
        checks.push(Check::gated("open", name.to_string(), max, "ms", budget::OPEN_MS, 1.0, format!("worst of {} {what}; p50 {p50:.2} ms", runs.len())));
        Ok(())
    };
    println!("\nopen (Store::thread, conversations):");
    time_open("plain text", &plain, "single messages")?;
    time_open("HTML, first open", &html, "newsletters (HTML → Markdown + layout tree)")?;
    time_open("HTML, reopen", &html, "newsletters, Markdown cached")?;
    time_open("conversation ≥6", &threads, "conversations, all bodies")?;
    Ok(())
}

pub fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}
