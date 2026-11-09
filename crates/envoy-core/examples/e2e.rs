//! End-to-end check of the engine against a real IMAP/SMTP server, GreenMail:
//!
//! ```sh
//! docker run -d --rm --name envoy-greenmail \
//!   -e GREENMAIL_OPTS='-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:pw@localhost,erin:pw@localhost -Dgreenmail.users.login=email' \
//!   -p 3025:3025 -p 3143:3143 -p 3465:3465 -p 3993:3993 greenmail/standalone:2.1.3
//! cargo run -p envoy-core --example e2e            # both passes; `-- full` or `-- plain` runs one
//! ```
//!
//! The scenario runs twice, each pass in a process of its own, with its own data folder and
//! GreenMail user, whose mailboxes it empties first (so it can run again on the same server):
//!
//! - `full` (alice): GreenMail as it is, IMAP with UIDPLUS, MOVE and IDLE.
//! - `plain` (erin): through a proxy that takes UIDPLUS and MOVE out of what the server says (its
//!   CAPABILITY, APPENDUID and COPYUID), so Envoy meets a server without them: moves are COPY then
//!   \Deleted, deletions only flag, and a draft's older copies are found by its Message-ID. The
//!   proxy fails the pass if Envoy sends what such a server lacks (MOVE, UID EXPUNGE), or EXPUNGE
//!   or CLOSE, which destroy every message flagged \Deleted, other clients' too. It makes its TLS
//!   certificate with `openssl`.
//!
//! Undo windows are as shipped (5 s), and new mail is checked for every five minutes, so mail that
//! arrives in seconds came by IDLE. GreenMail's IDLE reports new and expunged mail but not flag
//! changes, so flags set elsewhere are checked after a manual sync.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::engine::{Command, Engine, Event, MoveTarget};
use envoy_core::model::{Draft, Role, Summary, View, flags};
use envoy_core::settings::Settings;
use envoy_core::snooze;
use futures::TryStreamExt;
use lettre::message::{Attachment, MultiPart, header::ContentType};
use lettre::{Message, SmtpTransport, Transport};

/// GreenMail's ports (`-Dgreenmail.setup.test.all`).
const SMTP: u16 = 3025;
const SMTPS: u16 = 3465;
const IMAP: u16 = 3143;
const IMAPS: u16 = 3993;
const PASSWORD: &str = "pw";
const ACCOUNT: &str = "me";

/// Seconds an undoable move (or draft discard) waits before it may reach the server: the 5 s
/// window and its second of grace. A send waits the 5 s.
const MOVE_HOLD: i64 = 6;
const SEND_HOLD: i64 = 5;
/// How long after its deadline a held op may take to reach the server. The worker wakes on the
/// deadline, so it's a second or so; the rest is slack for a slow CI runner.
const LATE: f64 = 15.0;

/// Sent by the Envoy that's killed while it waits out the undo window ([`hold`]).
const QUEUED: &str = "Queued before a crash";

/// Mail Envoy must never touch, `(mailbox, flags, subject)`: messages another client flagged
/// \Deleted and hasn't expunged (an EXPUNGE or CLOSE from Envoy would destroy them), and a draft
/// written on another device.
const BYSTANDERS: &[(&str, &str, &str)] = &[
    ("INBOX", "(\\Deleted)", "Deleted elsewhere"),
    ("Trash", "(\\Deleted)", "Deleted from Trash elsewhere"),
    ("Drafts", "(\\Draft \\Deleted)", "Old draft deleted elsewhere"),
    ("Drafts", "(\\Draft \\Seen)", "From my phone"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pass {
    Full,
    Plain,
}

impl Pass {
    fn parse(s: &str) -> Pass {
        match s {
            "full" => Pass::Full,
            "plain" => Pass::Plain,
            _ => usage(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Pass::Full => "full",
            Pass::Plain => "plain",
        }
    }

    fn user(self) -> &'static str {
        match self {
            Pass::Full => "alice@localhost",
            Pass::Plain => "erin@localhost",
        }
    }
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            for pass in ["full", "plain"] {
                println!("\n━━ {pass} ━━");
                let status = std::process::Command::new(std::env::current_exe().unwrap()).arg(pass).status().unwrap();
                if !status.success() {
                    eprintln!("\nthe {pass} pass failed");
                    std::process::exit(1);
                }
            }
            println!("\nALL GOOD: both passes");
        }
        [pass] => scenario(Pass::parse(pass)),
        ["hold", pass] => hold(Pass::parse(pass)),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: e2e [full | plain]");
    std::process::exit(2);
}

fn settings() -> Settings {
    let mut s = Settings::default();
    for (key, v) in [("behavior.undoSeconds", 5), ("compose.undoSendSeconds", 5), ("sync.intervalSeconds", 300)] {
        s.set(key, serde_json::json!(v)).unwrap();
    }
    s
}

fn inbox_view() -> View {
    View::Role { account: Some(ACCOUNT.into()), role: Role::Inbox }
}

fn scenario(pass: Pass) {
    let home = std::env::temp_dir().join(format!("envoy-e2e-{}-{}", pass.name(), std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: single-threaded at this point; set before anything reads the environment. The
    // Envoy killed mid-hold inherits them.
    unsafe {
        std::env::set_var("ENVOY_HOME", &home);
        std::env::set_var("ENVOY_INSECURE_TLS", "1");
        // Keep the test's password out of the system keychain.
        std::env::set_var("ENVOY_SECRETS", "file");
    }
    let user = pass.user();
    let probe = Probe::new(user);
    probe.wait_ready();
    let proxy = (pass == Pass::Plain).then(|| Proxy::start(&home));

    println!("seeding {user}'s mailboxes…");
    probe.reset(&["Trash", "Sent", "Archive", "Junk", "Drafts", "Old Projects", "Receipts"]);
    for i in 0..3 {
        probe.append("Junk", "", &raw(user, &format!("Win a prize {i}"), &format!("<spam-{i}@x>")));
    }
    for (i, (mailbox, flags, subject)) in BYSTANDERS.iter().enumerate() {
        probe.append(mailbox, flags, &raw(user, subject, &format!("<bystander-{i}@elsewhere>")));
    }
    deliver(user, "Plans for Thursday", "Bob <bob@localhost>", "Can we meet Thursday at 2?", |b| b.message_id(Some("<plan-1@localhost>".into())));
    deliver(user, "Re: Plans for Thursday", "Carol <carol@localhost>", "Works for me.\n\nOn Mon, Bob wrote:\n> Can we meet?", |b| {
        b.message_id(Some("<plan-2@localhost>".into())).in_reply_to("<plan-1@localhost>".into()).references("<plan-1@localhost>".into())
    });
    let html = Message::builder()
        .from("News <news@localhost>".parse().unwrap())
        .to(user.parse().unwrap())
        .subject("Weekly pancake digest")
        .multipart(
            MultiPart::mixed()
                .multipart(MultiPart::alternative_plain_html("Pancakes are great.".to_owned(), "<html><body><h1>Digest</h1><p>Pancakes are <b>great</b>.</p><a href=\"https://example.com\">Read more</a></body></html>".to_owned()))
                .singlepart(Attachment::new("recipe.txt".into()).body(b"flour, eggs, milk".to_vec(), ContentType::TEXT_PLAIN)),
        )
        .unwrap();
    smtp().send(&html).unwrap();
    eventually("the seeded mail", 20, || (probe.live("INBOX").len() == 3).then_some(()));

    let mut d = Driver::new(Engine::start(|| {}, settings()).expect("engine"));
    let account = Account {
        id: ACCOUNT.into(),
        email: user.into(),
        display_name: "Me".into(),
        auth: AuthKind::Password,
        username: user.into(),
        imap_host: "localhost".into(),
        imap_port: proxy.as_ref().map_or(IMAPS, |p| p.port),
        smtp_host: "localhost".into(),
        smtp_port: SMTPS,
        hue: 0.0,
        signature: String::new(),
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
    };

    // ── Initial sync ──
    let t0 = Instant::now();
    let mark = d.mark();
    d.send(Command::AddAccount { account, secret: PASSWORD.into() });
    d.wait(mark, "account added", 20, |e| matches!(e, Event::AccountAdded(_)).then_some(()));
    println!("✓ account added ({:?})", t0.elapsed());

    let inbox = inbox_view();
    d.send(Command::Watch { view: inbox.clone() });
    let mailboxes = d.wait(mark, "mailboxes", 20, |e| match e {
        Event::Mailboxes { mailboxes, .. } if mailboxes.iter().any(|m| m.role == Role::Inbox) => Some(mailboxes.clone()),
        _ => None,
    });
    println!("✓ mailboxes: {:?}", mailboxes.iter().map(|m| format!("{}={:?}", m.name, m.role)).collect::<Vec<_>>());
    for (role, name) in [(Role::Trash, "Trash"), (Role::Sent, "Sent"), (Role::Archive, "Archive"), (Role::Junk, "Junk"), (Role::Drafts, "Drafts")] {
        assert!(mailboxes.iter().any(|m| m.role == role && m.name == name), "{name} found by name");
    }

    let items = d.until("three messages synced", 20, |d| Some(d.list(&inbox, "", false)).filter(|i| i.len() >= 3));
    assert_eq!(sorted(subjects(&items)), ["Plans for Thursday", "Re: Plans for Thursday", "Weekly pancake digest"], "the inbox, without what's flagged \\Deleted");
    println!("✓ synced {} messages ({:?} since add)", items.len(), t0.elapsed());

    let conv = d.list(&inbox, "", true);
    assert_eq!(conv.len(), 2, "two conversations");
    let plans = conv.iter().find(|s| s.thread_count == 2).expect("a two-message conversation").clone();
    println!("✓ conversations: {} rows, thread participants \"{}\"", conv.len(), plans.participants);

    // Open the HTML message: body arrives (prefetched or on demand), with markdown + attachment.
    let digest = items.iter().find(|s| s.subject.contains("pancake")).unwrap().clone();
    d.req += 1;
    let (req, mark) = (d.req, d.mark());
    d.send(Command::Open { req, id: digest.id, conversations: true });
    let body = d.wait(mark, "body", 20, |e| match e {
        Event::Thread { req: r, messages, .. } if *r == req => messages.last().and_then(|m| m.body.clone()),
        _ => None,
    });
    let md = body.markdown.clone().unwrap_or_default();
    assert!(md.contains("**great**"), "markdown: {md}");
    assert_eq!(body.attachments.len(), 1);
    println!("✓ opened HTML message: markdown {:?}…, attachment {}", &md[..md.len().min(40)], body.attachments[0].filename);

    // Search (FTS on body text, after bodies download).
    d.until("search finds body text", 10, |d| d.list(&inbox, "pancakes", false).iter().any(|h| h.id == digest.id).then_some(()));
    assert_eq!(d.list(&inbox, "from:carol", false).len(), 1);
    println!("✓ search: body text and from: filter");

    // ── Flags, both ways ──
    d.send(Command::SetFlags { ids: vec![digest.id], add: flags::SEEN | flags::FLAGGED, remove: 0, thread: false });
    d.until("read and starred on the server", 15, |_| probe.flags("INBOX", &digest.subject).filter(|f| f & (flags::SEEN | flags::FLAGGED) == flags::SEEN | flags::FLAGGED));
    d.send(Command::SetFlags { ids: vec![digest.id], add: 0, remove: flags::FLAGGED, thread: false });
    d.until("unstarred on the server", 15, |_| probe.flags("INBOX", &digest.subject).filter(|f| f & (flags::SEEN | flags::FLAGGED) == flags::SEEN));
    println!("✓ read, starred, unstarred: each reached the server");
    // Read and starred on another device.
    let first = items.iter().find(|s| s.subject == "Plans for Thursday").unwrap().clone();
    assert!(!first.seen() && !first.flagged());
    probe.store("INBOX", first.uid, "+FLAGS (\\Seen \\Flagged)");
    d.send(Command::SyncNow);
    d.until("flags set elsewhere", 15, |d| d.list(&inbox, "", false).into_iter().find(|s| s.id == first.id).filter(|s| s.seen() && s.flagged()));
    println!("✓ read and starred elsewhere: shown here after a sync");

    // ── Moves, held for the undo window ──
    // Spam, undone within the window: back in the inbox, and the server never moves it.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Move { ids: vec![digest.id], to: MoveTarget::Junk, thread: false, undo: Some(1) });
    d.wait(mark, "the move held", 5, |e| matches!(e, Event::Held { undo: 1, ops: 1 }).then_some(()));
    let due = deadline("batch = 1");
    assert_hold("the spam move", due, before, epoch(), MOVE_HOLD);
    assert!(!d.list(&inbox, "", false).iter().any(|s| s.id == digest.id), "gone from the inbox at once");
    d.send(Command::UndoMove { undo: 1 });
    assert!(d.list(&inbox, "", false).iter().any(|s| s.id == digest.id), "back after undo");
    untouched_until(&mut d, "the server saw the undone move", due as f64 + 3.0, || probe.live("Junk").len() == 3 && probe.live("INBOX").contains(&digest.subject));
    println!("✓ spam undone: message back, server untouched past the window");

    // Archive the conversation, undoable: it waits out the window, then reaches the server.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Move { ids: vec![plans.id], to: MoveTarget::Archive, thread: true, undo: Some(2) });
    d.wait(mark, "the move held", 5, |e| matches!(e, Event::Held { undo: 2, ops: 2 }).then_some(()));
    let due = deadline("batch = 2");
    assert_hold("the archive", due, before, epoch(), MOVE_HOLD);
    watch_holds(&mut d, vec![Hold { what: "the archive", deadline: due, on_server: Box::new(|| !probe.live("Archive").is_empty() || probe.live("INBOX").len() != 3) }]);
    d.until("the whole conversation in Archive", 10, |_| (probe.live("Archive").len() == 2 && probe.live("INBOX").len() == 1).then_some(()));
    println!("✓ archived conversation: 2 messages in Archive, after the window and not before");
    assert_eq!(d.list(&inbox, "", false).len(), 1, "inbox shows one message locally");
    let mark = d.mark();
    d.send(Command::UndoMove { undo: 2 });
    d.wait(mark, "too-late toast", 5, |e| matches!(e, Event::Toast(t) if t.starts_with("Too late")).then_some(()));
    println!("✓ undo after the window says it's too late");

    // Opening a folder that isn't synced yet: the foreground connection brings it in at once.
    let junk = View::Mailbox { account: ACCOUNT.into(), mailbox: "Junk".into() };
    let t_junk = Instant::now();
    d.send(Command::Watch { view: junk.clone() });
    d.until("Junk synced", 10, |d| (d.list(&junk, "", false).len() == 3).then_some(()));
    println!("✓ opened an unsynced folder: 3 messages in {:?}", t_junk.elapsed());
    // Filed into the open folder by another client while it's open: shown at the next look at
    // it (every 30 s), well before the next full sync (five minutes off), with nothing new in
    // the inbox for IDLE to wake on.
    let t_filed = Instant::now();
    probe.append("Junk", "", &raw(user, "Filed here elsewhere", "<filed-1@elsewhere>"));
    d.until("the message filed into the open folder", 45, |d| d.list(&junk, "", false).iter().any(|s| s.subject == "Filed here elsewhere").then_some(()));
    println!("✓ filed into the open folder elsewhere: shown in {:?}", t_filed.elapsed());

    // ── Push: a new message arrives while idle ──
    let t_push = Instant::now();
    deliver(user, "Ping while idle", "Dan <dan@localhost>", "IDLE should bring this in.", |b| b);
    let ping = d.until("the IDLE push", 20, |d| d.list(&inbox, "", false).into_iter().find(|s| s.subject == "Ping while idle"));
    println!("✓ new mail pushed via IDLE in {:?}", t_push.elapsed());

    // Moved to a folder whose name has a space, undoable: it lands after the window.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Move { ids: vec![ping.id], to: MoveTarget::Mailbox("Old Projects".into()), thread: false, undo: Some(3) });
    d.wait(mark, "the move held", 5, |e| matches!(e, Event::Held { undo: 3, ops: 1 }).then_some(()));
    let due = deadline("batch = 3");
    assert_hold("the move", due, before, epoch(), MOVE_HOLD);
    watch_holds(&mut d, vec![Hold { what: "the move", deadline: due, on_server: Box::new(|| !probe.live("Old Projects").is_empty() || !probe.live("INBOX").contains(&ping.subject)) }]);
    d.until("the move on the server", 10, |_| (probe.live("Old Projects") == [ping.subject.clone()] && !probe.live("INBOX").contains(&ping.subject)).then_some(()));
    println!("✓ moved to \"Old Projects\", after the window and not before");

    // ── Labels and moves of several messages at once ──
    // A label is a copy in the label's mailbox (Gmail's labels are mailboxes; GreenMail's
    // folders do for the check), the message staying where it is. Two messages moved at once
    // (a multi-select) wait under one undo key. Each is undone first: nothing reaches the server.
    deliver(user, "Batch one", "Eve <eve@localhost>", "First of two.", |b| b);
    deliver(user, "Batch two", "Eve <eve@localhost>", "Second of two.", |b| b);
    let batch = d.until("the two to move", 20, |d| {
        let ids: Vec<_> = d.list(&inbox, "", false).iter().filter(|s| s.subject.starts_with("Batch ")).map(|s| s.id).collect();
        (ids.len() == 2).then_some(ids)
    });
    let in_inbox = |d: &mut Driver, ids: &[i64]| d.list(&inbox, "", false).iter().filter(|s| ids.contains(&s.id)).count();
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Copy { ids: vec![digest.id], mailbox: "Receipts".into(), thread: false, undo: Some(30) });
    d.send(Command::Move { ids: batch.clone(), to: MoveTarget::Mailbox("Old Projects".into()), thread: false, undo: Some(32) });
    d.wait(mark, "the label held", 5, |e| matches!(e, Event::Held { undo: 30, ops: 1 }).then_some(()));
    d.wait(mark, "the batch held", 5, |e| matches!(e, Event::Held { undo: 32, ops: 2 }).then_some(()));
    let (label_undone, batch_undone) = (deadline("batch = 30"), deadline("batch = 32"));
    assert_hold("the label", label_undone, before, epoch(), MOVE_HOLD);
    assert_hold("the batch move", batch_undone, before, epoch(), MOVE_HOLD);
    assert_eq!((in_inbox(&mut d, &batch), in_inbox(&mut d, &[digest.id])), (0, 1), "the batch leaves the inbox at once, the labeled message stays");
    d.send(Command::UndoMove { undo: 30 });
    d.send(Command::UndoMove { undo: 32 });
    assert_eq!(in_inbox(&mut d, &batch), 2, "one undo brings both back");
    untouched_until(&mut d, "the undone label or batch reached the server", label_undone.max(batch_undone) as f64 + 3.0, || {
        probe.live("Receipts").is_empty() && probe.live("Old Projects") == [ping.subject.clone()] && probe.live("INBOX").iter().filter(|s| s.starts_with("Batch ")).count() == 2
    });
    println!("✓ a label and a two-message move, undone: back here, the server untouched past the window");
    // Then for good: after the window, one copy in Receipts and the message still in the inbox;
    // both moved messages in Old Projects.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Copy { ids: vec![digest.id], mailbox: "Receipts".into(), thread: false, undo: Some(31) });
    d.send(Command::Move { ids: batch.clone(), to: MoveTarget::Mailbox("Old Projects".into()), thread: false, undo: Some(33) });
    d.wait(mark, "the label held", 5, |e| matches!(e, Event::Held { undo: 31, ops: 1 }).then_some(()));
    d.wait(mark, "the batch held", 5, |e| matches!(e, Event::Held { undo: 33, ops: 2 }).then_some(()));
    let (label_due, batch_due) = (deadline("batch = 31"), deadline("batch = 33"));
    assert_hold("the label", label_due, before, epoch(), MOVE_HOLD);
    assert_hold("the batch move", batch_due, before, epoch(), MOVE_HOLD);
    watch_holds(
        &mut d,
        vec![
            Hold { what: "the label", deadline: label_due, on_server: Box::new(|| !probe.live("Receipts").is_empty()) },
            Hold { what: "the batch move", deadline: batch_due, on_server: Box::new(|| probe.live("Old Projects").len() > 1 || probe.live("INBOX").iter().filter(|s| s.starts_with("Batch ")).count() < 2) },
        ],
    );
    d.until("the label and the batch on the server", 10, |_| {
        let moved = sorted(probe.live("Old Projects")) == sorted(vec!["Batch one".to_owned(), "Batch two".to_owned(), ping.subject.clone()]);
        let labeled = probe.live("Receipts") == [digest.subject.clone()] && probe.live("INBOX").contains(&digest.subject);
        (moved && labeled && !probe.live("INBOX").iter().any(|s| s.starts_with("Batch "))).then_some(())
    });
    assert_eq!(in_inbox(&mut d, &[digest.id]), 1, "labeled, still in the inbox here too");
    println!("✓ labeled: one copy in Receipts, the message where it was; both moved messages in Old Projects");

    // ── Sending, with undo ──
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Send { draft: Draft { account: ACCOUNT.into(), to: user.into(), subject: "Take this back".into(), body: "Oops.".into(), ..Draft::default() }, draft_id: None });
    let op = d.wait(mark, "the send queued", 10, |e| match e {
        Event::Sent { op, undo_secs: 5, draft_id: None } => Some(*op),
        _ => None,
    });
    let undone = deadline(&format!("id = {op}"));
    assert_hold("the send", undone, before, epoch(), SEND_HOLD);
    let mark = d.mark();
    d.send(Command::UndoSend { op });
    d.wait(mark, "send undone", 5, |e| matches!(e, Event::Toast(t) if t == "Sending undone").then_some(()));
    // Sent through the engine (to ourselves), with an Answered flag on the original.
    let reply = Draft { account: ACCOUNT.into(), to: format!("Me <{user}>"), subject: "Sent from Envoy".into(), body: "Hello from the e2e test.".into(), reply_of: Some(digest.id), ..Draft::default() };
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Send { draft: reply, draft_id: None });
    let op = d.wait(mark, "the send queued", 10, |e| match e {
        Event::Sent { op, undo_secs: 5, .. } => Some(*op),
        _ => None,
    });
    let due = deadline(&format!("id = {op}"));
    assert_hold("the send", due, before, epoch(), SEND_HOLD);
    watch_holds(&mut d, vec![Hold { what: "the send", deadline: due, on_server: Box::new(|| !probe.live("Sent").is_empty() || probe.live("INBOX").iter().any(|s| s == "Sent from Envoy")) }]);
    d.wait(mark, "sent toast", 30, |e| matches!(e, Event::Toast(t) if t == "Message sent").then_some(()));
    d.until("the message received back", 20, |d| d.list(&inbox, "", false).iter().any(|s| s.subject == "Sent from Envoy").then_some(()));
    println!("✓ sent via SMTP after the undo window and not before, received back");
    d.until("a copy in Sent, and the original answered", 15, |_| {
        (probe.live("Sent") == ["Sent from Envoy"] && probe.flags("INBOX", &digest.subject).is_some_and(|f| f & flags::ANSWERED != 0)).then_some(())
    });
    untouched_until(&mut d, "the undone send went out", undone as f64 + 3.0, || !probe.live("INBOX").iter().any(|s| s == "Take this back"));
    println!("✓ copy appended to Sent, original marked answered; the undone send never went out");

    // ── Drafts: saved as they're typed, on the server in Drafts, one copy however many edits ──
    let t_draft = Instant::now();
    let mut draft = Draft { account: ACCOUNT.into(), to: format!("Me <{user}>"), bcc: "bob@localhost".into(), subject: "Draft one".into(), ..Draft::default() };
    for (rev, words) in [(1, "First words."), (2, "First words, more."), (3, "First words, more, and the last.")] {
        draft.body = words.into();
        d.send(Command::SaveDraft { id: 1, draft: draft.clone(), open: true, rev, lifecycle: 0 });
    }
    d.until("one copy in Drafts, the newest", 30, |_| {
        let mine = probe.drafts();
        (mine.len() == 1 && mine[0].0 == "Draft one" && mine[0].1.contains("the last")).then_some(())
    });
    println!("✓ three quick saves: one copy in Drafts, the newest ({:?})", t_draft.elapsed());
    // Edited and put away: replaced, not added, and at once.
    draft.subject = "Draft one, edited".into();
    d.send(Command::SaveDraft { id: 1, draft: draft.clone(), open: false, rev: 4, lifecycle: 0 });
    d.until("the edit in Drafts", 30, |_| (probe.drafts().iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>() == ["Draft one, edited"]).then_some(()));
    println!("✓ an edit replaces the copy in Drafts");
    // Listed once, as its server copy once that's synced, beside the draft from another device.
    let drafts_view = View::Role { account: Some(ACCOUNT.into()), role: Role::Drafts };
    d.send(Command::Watch { view: drafts_view.clone() });
    let row = d.until("the server copy listed as the draft", 20, |d| {
        let rows = d.list(&drafts_view, "", false);
        let mine: Vec<&Summary> = rows.iter().filter(|r| r.subject == "Draft one, edited").collect();
        assert!(mine.len() <= 1 && rows.iter().any(|r| r.subject == "From my phone"), "listed once, beside the other device's: {:?}", subjects(&rows));
        mine.first().filter(|r| r.id > 0).map(|r| (*r).clone())
    });
    let mark = d.mark();
    d.send(Command::OpenDraft { id: row.id });
    let opened = d.wait(mark, "draft opened", 10, |e| match e {
        Event::DraftOpened { id, draft, .. } => Some((*id, draft.clone())),
        _ => None,
    });
    assert_eq!((opened.0, opened.1.bcc.as_str()), (1, "bob@localhost"), "opens as the local draft, Bcc and all");
    println!("✓ listed once; opens as the local draft");
    // Sent (to us only: GreenMail has no Bob), then undone: back in the composer, and its copy in
    // Drafts never goes, all through the window.
    draft.bcc.clear();
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Send { draft: draft.clone(), draft_id: Some(1) });
    let op = d.wait(mark, "the send queued", 10, |e| match e {
        Event::Sent { op, draft_id: Some(1), .. } => Some(*op),
        _ => None,
    });
    let undone = deadline(&format!("id = {op}"));
    assert_hold("the draft's send", undone, before, epoch(), SEND_HOLD);
    let mark = d.mark();
    d.send(Command::UndoSend { op });
    let back = d.wait(mark, "the draft back in the composer", 5, |e| match e {
        Event::DraftOpened { id: 1, draft, .. } => Some(draft.clone()),
        _ => None,
    });
    assert_eq!(back.subject, "Draft one, edited");
    untouched_until(&mut d, "the undone send went out, or its draft left Drafts", undone as f64 + 3.0, || {
        !probe.drafts().is_empty() && !probe.live("INBOX").iter().any(|s| s == "Draft one, edited")
    });
    d.until("one copy in Drafts", 20, |_| (probe.drafts().len() == 1).then_some(()));
    println!("✓ undo send: the draft back, its copy kept in Drafts, nothing sent");
    // Sent for good: the message goes out and the draft leaves Drafts.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Send { draft: draft.clone(), draft_id: Some(1) });
    let op = d.wait(mark, "the send queued", 10, |e| match e {
        Event::Sent { op, draft_id: Some(1), .. } => Some(*op),
        _ => None,
    });
    let due = deadline(&format!("id = {op}"));
    assert_hold("the draft's send", due, before, epoch(), SEND_HOLD);
    watch_holds(&mut d, vec![Hold { what: "the draft's send", deadline: due, on_server: Box::new(|| probe.live("INBOX").iter().any(|s| s == "Draft one, edited")) }]);
    d.until("the sent draft gone from Drafts", 20, |_| probe.drafts().is_empty().then_some(()));
    println!("✓ sending the draft removed it from Drafts");
    // Discarded, then undone within the window: the server never loses it. Then discarded for good.
    let other = Draft { account: ACCOUNT.into(), subject: "Never mind".into(), body: "Scratch that.".into(), ..Draft::default() };
    d.send(Command::SaveDraft { id: 2, draft: other, open: false, rev: 1, lifecycle: 0 });
    d.until("the draft in Drafts", 30, |_| (probe.drafts().iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>() == ["Never mind"]).then_some(()));
    let before = epoch();
    d.send(Command::DiscardDraft { id: 2, undo: Some(4) });
    assert!(!subjects(&d.list(&drafts_view, "", false)).contains(&"Never mind"), "gone from the list at once");
    let undone = deadline("batch = 4");
    assert_hold("the discard", undone, before, epoch(), MOVE_HOLD);
    d.send(Command::UndoMove { undo: 4 });
    assert!(subjects(&d.list(&drafts_view, "", false)).contains(&"Never mind"), "back after undo");
    untouched_until(&mut d, "the server saw the undone discard", undone as f64 + 3.0, || probe.drafts().len() == 1);
    d.send(Command::DiscardDraft { id: 2, undo: None });
    d.until("the discarded draft gone from Drafts", 30, |_| probe.drafts().is_empty().then_some(()));
    println!("✓ discarding a draft: undone, server untouched; then gone from Drafts");

    // ── Trash, then delete forever ──
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Move { ids: vec![digest.id], to: MoveTarget::Trash, thread: false, undo: Some(5) });
    d.wait(mark, "the move held", 5, |e| matches!(e, Event::Held { undo: 5, ops: 1 }).then_some(()));
    let due = deadline("batch = 5");
    assert_hold("the delete", due, before, epoch(), MOVE_HOLD);
    watch_holds(&mut d, vec![Hold { what: "the delete", deadline: due, on_server: Box::new(|| !probe.live("Trash").is_empty() || !probe.live("INBOX").contains(&digest.subject)) }]);
    d.until("in Trash on the server", 10, |_| (probe.live("Trash") == [digest.subject.clone()] && !probe.live("INBOX").contains(&digest.subject)).then_some(()));
    println!("✓ deleted to Trash, after the window and not before");
    // The foreground connection has sat idle about 30 s here, GreenMail's limit, so it's often
    // gone: that must cost a fresh connection, not this wait (before, a SELECT on the closed
    // connection read as an empty Trash, and nothing synced it again for the 300 s interval).
    let trash = View::Role { account: Some(ACCOUNT.into()), role: Role::Trash };
    d.send(Command::Watch { view: trash.clone() });
    let trashed = d.until("the message in Trash here", 20, |d| d.list(&trash, "", false).into_iter().find(|s| s.subject == digest.subject));
    let mark = d.mark();
    d.send(Command::Move { ids: vec![trashed.id], to: MoveTarget::Trash, thread: false, undo: Some(6) });
    let held = d.wait(mark, "the delete", 5, |e| match e {
        Event::Held { undo: 6, ops } => Some(*ops),
        _ => None,
    });
    assert_eq!(held, 0, "deleting from Trash is permanent: nothing to undo");
    assert!(!d.list(&trash, "", false).iter().any(|s| s.id == trashed.id), "gone here at once");
    d.until("gone from the server", 15, |_| {
        let left = probe.live("Trash").is_empty();
        match pass {
            // UID EXPUNGE: gone for real, and only it (the other client's \Deleted mail stays).
            Pass::Full => (left && probe.count("Trash") == 1).then_some(()),
            // Flagged \Deleted, for the server or the user to expunge.
            Pass::Plain => (left && probe.deleted("Trash").contains(&digest.subject)).then_some(()),
        }
    });
    println!("✓ deleted forever from Trash");

    // ── Killed while moves and a send wait out their window: they happen on the next start ──
    drop(d);
    std::thread::sleep(Duration::from_secs(1));
    let [move_before, move_after, send_before, send_after] = kill_mid_hold(pass);
    let move_due = deadline("batch = 100");
    let send_due = deadline("op LIKE '{\"kind\":\"send\"%'");
    assert_hold("the move held at the kill", move_due, move_before, move_after, MOVE_HOLD);
    assert_hold("the send held at the kill", send_due, send_before, send_after, SEND_HOLD);
    assert!(probe.live("INBOX").iter().any(|s| s == "Sent from Envoy") && !probe.live("Archive").iter().any(|s| s == "Sent from Envoy"), "the held move stayed off the server");
    assert!(!probe.live("INBOX").iter().any(|s| s == QUEUED), "the held send stayed unsent");
    println!("✓ killed mid-hold: nothing reached the server");
    let t_cold = Instant::now();
    let mut d = Driver::new(Engine::start(|| {}, settings()).unwrap());
    let items = d.list(&inbox, "", false);
    println!("✓ restart: {} inbox messages from the cache in {:?} (before any network)", items.len(), t_cold.elapsed());
    assert_eq!(subjects(&items), ["Draft one, edited"], "the held message stays hidden across the restart");
    // What's left of each window is waited out after the restart, not skipped.
    let restarted = epoch();
    assert!(restarted + 1.0 < send_due.min(move_due) as f64, "restarted {:.1} s before the first deadline: too late to watch the rest of the windows", send_due.min(move_due) as f64 - restarted);
    watch_holds(
        &mut d,
        vec![
            Hold { what: "the move held across the restart", deadline: move_due, on_server: Box::new(|| probe.live("Archive").iter().any(|s| s == "Sent from Envoy") || !probe.live("INBOX").iter().any(|s| s == "Sent from Envoy")) },
            Hold { what: "the send held across the restart", deadline: send_due, on_server: Box::new(|| probe.live("INBOX").iter().any(|s| s == QUEUED) || probe.live("Sent").iter().any(|s| s == QUEUED)) },
        ],
    );
    d.until("the held send's copy in Sent", 15, |_| (probe.live("INBOX").iter().any(|s| s == QUEUED) && probe.live("Sent").iter().any(|s| s == QUEUED)).then_some(()));
    println!("✓ the held move and send went through after the restart, at the end of their windows");
    d.until("an empty queue", 30, |d| d.events.iter().rev().find_map(|e| if let Event::Pending(n) = e { Some(*n) } else { None }).filter(|n| *n == 0));
    println!("✓ nothing left queued");

    // ── Attachments: read from disk into a draft, in its Drafts copy, sent; a forward takes them ──
    let name = "Plán — 2026.pdf";
    let pdf: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let path = home.join(name);
    std::fs::write(&path, &pdf).unwrap();
    let has_file = |mailbox: &str, subject: &str| probe.files(mailbox, subject) == [(name.to_owned(), pdf.clone())];
    let mut with_file = Draft { account: ACCOUNT.into(), to: format!("Me <{user}>"), subject: "With a file".into(), body: "Attached.".into(), ..Draft::default() };
    // Saved first: a file is kept with its draft.
    d.send(Command::SaveDraft { id: 3, draft: with_file.clone(), open: true, rev: 1, lifecycle: 0 });
    let mark = d.mark();
    d.send(Command::AttachFiles { draft: 3, files: vec![(0, path)] });
    let attached = d.wait(mark, "the file attached", 10, |e| match e {
        Event::FileAttached { draft: 3, file } => Some(file.clone()),
        Event::AttachFailed { error, .. } => panic!("not attached: {error}"),
        _ => None,
    });
    assert_eq!((attached.filename.as_str(), attached.mime.as_str(), attached.size), (name, "application/pdf", pdf.len()));
    with_file.attachments = vec![attached];
    d.send(Command::SaveDraft { id: 3, draft: with_file.clone(), open: false, rev: 2, lifecycle: 0 });
    d.until("the draft in Drafts with its file", 30, |_| has_file("Drafts", "With a file").then_some(()));
    println!("✓ a file read into a draft, and in its Drafts copy: name and bytes");
    let mark = d.mark();
    d.send(Command::Send { draft: with_file, draft_id: Some(3) });
    d.wait(mark, "the send queued", 10, |e| matches!(e, Event::Sent { draft_id: Some(3), .. }).then_some(()));
    d.until("the message received with its file", 30, |_| has_file("INBOX", "With a file").then_some(()));
    d.until("the sent draft gone from Drafts", 20, |_| probe.drafts().is_empty().then_some(()));
    println!("✓ sent with the file: received, name and bytes intact; the draft gone from Drafts");
    // Forwarded: the original's file goes along, fetched from the server if it isn't here yet.
    let received = d.until("the message synced", 30, |d| d.list(&inbox, "", false).into_iter().find(|s| s.subject == "With a file"));
    let mut fwd = Draft { account: ACCOUNT.into(), to: format!("Me <{user}>"), subject: "Fwd: With a file".into(), body: "See below.".into(), ..Draft::default() };
    d.send(Command::SaveDraft { id: 4, draft: fwd.clone(), open: true, rev: 1, lifecycle: 0 });
    let mark = d.mark();
    let listed = envoy_core::model::DraftAttachment { id: 1, filename: name.into(), mime: "application/pdf".into(), size: pdf.len(), ..Default::default() };
    d.send(Command::AttachForwarded { draft: 4, message: received.id, files: vec![(listed, 0)] });
    let forwarded = d.wait(mark, "the original's file attached", 30, |e| match e {
        Event::FileAttached { draft: 4, file } => Some(file.clone()),
        Event::AttachFailed { error, .. } => panic!("not attached: {error}"),
        _ => None,
    });
    fwd.attachments = vec![forwarded];
    d.send(Command::SaveDraft { id: 4, draft: fwd.clone(), open: true, rev: 2, lifecycle: 0 });
    d.send(Command::Send { draft: fwd, draft_id: Some(4) });
    d.until("the forward received with the file", 30, |_| has_file("INBOX", "Fwd: With a file").then_some(()));
    println!("✓ forwarded with the original's file");
    d.until("an empty queue", 30, |d| d.events.iter().rev().find_map(|e| if let Event::Pending(n) = e { Some(*n) } else { None }).filter(|n| *n == 0));

    // ── Snoozes: kept on the server, in mailboxes named for when they wake ──
    snoozes(&mut d, &probe, pass, user, &inbox);

    // ── What Envoy must never have touched ──
    for (mailbox, flags, subject) in BYSTANDERS {
        let found = probe.messages(mailbox).into_iter().find(|m| m.subject == *subject);
        let found = found.unwrap_or_else(|| panic!("{subject:?} in {mailbox} was destroyed"));
        assert_eq!(found.flags & flags::DELETED != 0, flags.contains("Deleted"), "{subject:?} in {mailbox}");
    }
    match &proxy {
        None => {
            for mailbox in ["INBOX", "Archive", "Trash", "Junk", "Sent", "Drafts", "Old Projects"] {
                let expected: Vec<&str> = BYSTANDERS.iter().filter(|(m, f, _)| *m == mailbox && f.contains("Deleted")).map(|(_, _, s)| *s).collect();
                assert_eq!(sorted(probe.deleted(mailbox)), sorted(expected), "{mailbox}: MOVE and UID EXPUNGE leave nothing of Envoy's flagged \\Deleted");
            }
        }
        Some(proxy) => {
            let violations = proxy.violations.lock().unwrap();
            assert!(violations.is_empty(), "sent to a server without UIDPLUS and MOVE: {violations:?}");
            let commands = proxy.commands.lock().unwrap();
            assert!(commands.iter().any(|c| c.starts_with("UID COPY")), "moves went by COPY");
            assert!(commands.iter().any(|c| c.starts_with("UID SEARCH")), "drafts were found by Message-ID");
            assert!(probe.deleted("INBOX").len() > 1, "what Envoy moved away is left flagged \\Deleted");
        }
    }
    println!("✓ mail flagged \\Deleted by others and the other device's draft: untouched");

    println!("\nALL GOOD ({} pass)", pass.name());
    drop(d);
    let _ = std::fs::remove_dir_all(&home);
}

/// Snoozing and waking against the server: a snooze undone inside its window never reaches it;
/// one let go lands in its bucket (made and subscribed), gone from the inbox; one whose time
/// comes before it reaches the server wakes where it is instead; a bucket another Envoy left
/// past its time wakes on the next check, unread and pinned here; an empty one, however old, is
/// never deleted (only Gmail's are, whose DELETE takes no mail); and Unsnooze from the Snoozed
/// view brings a message back.
fn snoozes(d: &mut Driver, probe: &Probe, pass: Pass, user: &str, inbox: &View) {
    deliver(user, "Snooze me", "Fay <fay@localhost>", "Later, please.", |b| b.message_id(Some("<snooze-1@localhost>".into())));
    deliver(user, "Snooze me back", "Fay <fay@localhost>", "Too soon to snooze.", |b| b.message_id(Some("<snooze-2@localhost>".into())));
    let rows = d.until("the two to snooze", 20, |d| {
        let rows: Vec<Summary> = d.list(inbox, "", false).into_iter().filter(|s| s.subject.starts_with("Snooze me")).collect();
        (rows.len() == 2).then_some(rows)
    });
    let later = rows.iter().find(|s| s.subject == "Snooze me").unwrap().clone();
    let soon = rows.iter().find(|s| s.subject == "Snooze me back").unwrap().clone();
    let until = (epoch() as i64 + 3 * 3600) / 60 * 60;
    // The bucket for a time, if it's on the server: this run's, whatever earlier runs left.
    let delimiter = probe.delimiter();
    let bucket_for = |at: i64| Some(snooze::bucket("Snoozed", &delimiter, at)).filter(|b| probe.names().contains(b));

    // Undone inside the window: back here, and the server never hears of it.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Snooze { ids: vec![later.id], until, thread: false, undo: Some(40) });
    d.wait(mark, "the snooze held", 5, |e| matches!(e, Event::Held { undo: 40, ops: 1 }).then_some(()));
    let undone = deadline("batch = 40");
    assert_hold("the snooze", undone, before, epoch(), MOVE_HOLD);
    assert!(!d.list(inbox, "", false).iter().any(|s| s.id == later.id), "gone from the inbox at once");
    d.send(Command::UndoMove { undo: 40 });
    assert!(d.list(inbox, "", false).iter().any(|s| s.id == later.id), "back after undo");
    untouched_until(d, "the undone snooze reached the server", undone as f64 + 3.0, || bucket_for(until).is_none() && probe.live("INBOX").iter().any(|s| s == "Snooze me"));
    println!("✓ snooze undone: back here, no bucket made, the server untouched past the window");

    // Let go: after the window, in its bucket (made, and subscribed), gone from the inbox.
    let (mark, before) = (d.mark(), epoch());
    d.send(Command::Snooze { ids: vec![later.id], until, thread: false, undo: Some(41) });
    d.wait(mark, "the snooze held", 5, |e| matches!(e, Event::Held { undo: 41, ops: 1 }).then_some(()));
    let due = deadline("batch = 41");
    assert_hold("the snooze", due, before, epoch(), MOVE_HOLD);
    watch_holds(d, vec![Hold { what: "the snooze", deadline: due, on_server: Box::new(|| bucket_for(until).is_some() || !probe.live("INBOX").iter().any(|s| s == "Snooze me")) }]);
    let bucket = d.until("in its bucket on the server, gone from the inbox", 15, |_| {
        let bucket = bucket_for(until)?;
        (probe.live(&bucket) == ["Snooze me"] && !probe.live("INBOX").iter().any(|s| s == "Snooze me")).then_some(bucket)
    });
    let parent = "Snoozed".to_owned();
    let subscribed = probe.subscribed();
    assert!(subscribed.contains(&parent) && subscribed.contains(&bucket), "subscribed, for clients that show only those: {subscribed:?}");
    println!("✓ snoozed: in {bucket} on the server after the window and not before, subscribed, gone from the inbox");

    // Its time comes before it reaches the server: it never goes into a bucket, and wakes where
    // it is, unread.
    d.send(Command::SetFlags { ids: vec![soon.id], add: flags::SEEN, remove: 0, thread: false });
    d.until("read on the server", 15, |_| probe.flags("INBOX", &soon.subject).filter(|f| f & flags::SEEN != 0));
    let (mark, too_soon) = (d.mark(), epoch() as i64 + 30);
    d.send(Command::Snooze { ids: vec![soon.id], until: too_soon, thread: false, undo: Some(42) });
    d.wait(mark, "the snooze held", 5, |e| matches!(e, Event::Held { undo: 42, ops: 1 }).then_some(()));
    d.until("woken in place: unread on the server", 30, |_| probe.flags("INBOX", &soon.subject).filter(|f| f & flags::SEEN == 0));
    let row = d.until("pinned here", 15, |d| d.list(inbox, "", false).into_iter().find(|s| s.id == soon.id && s.flags & flags::WOKEN != 0));
    assert!(!row.seen());
    assert!(bucket_for(too_soon).is_none(), "no bucket for a time that came");
    println!("✓ a snooze whose time came first: left in the inbox, unread and pinned, no bucket made");

    // A bucket another Envoy left past its time wakes on the next check (Check mail reads the
    // folders again): back in the inbox, unread, pinned atop it here. An old empty one stays: on
    // a server like this one DELETE would take a message another Envoy moved in meanwhile.
    let past = snooze::bucket("Snoozed", &delimiter, (epoch() as i64 - 2 * 3600) / 60 * 60);
    let old = snooze::bucket("Snoozed", &delimiter, (epoch() as i64 - 3 * 86_400) / 60 * 60);
    probe.create(&past);
    probe.create(&old);
    probe.append(&past, "(\\Seen)", &raw(user, "Back from a snooze", "<woke@elsewhere>"));
    d.send(Command::SyncNow);
    // Without MOVE each Envoy waits up to 30 s first.
    d.until("the due bucket woken into the inbox, unread", 60, |_| probe.flags("INBOX", "Back from a snooze").filter(|f| f & flags::SEEN == 0));
    // Pinned among the woken ("Snooze me back" is too), above the rest.
    let woke = d.until("pinned atop the inbox here", 20, |d| {
        let rows = d.list(inbox, "", false);
        let at = rows.iter().position(|s| s.subject == "Back from a snooze")?;
        rows[..=at].iter().all(|s| s.flags & flags::WOKEN != 0).then(|| rows[at].clone())
    });
    assert!(!woke.seen());
    match pass {
        // MOVE: nothing left behind.
        Pass::Full => assert_eq!(probe.count(&past), 0),
        // COPY, then flagged \Deleted, never expunged without UIDPLUS.
        Pass::Plain => assert_eq!(probe.deleted(&past), ["Back from a snooze"]),
    }
    // The round that woke it has looked at every bucket by now.
    let names = probe.names();
    assert!(names.contains(&old) && names.contains(&past), "buckets are never deleted off Gmail");
    println!("✓ a bucket past its time woke on the next check, pinned here; old empty ones stay");

    // Unsnooze from Snoozed: back in the inbox now.
    let snoozed = View::Role { account: Some(ACCOUNT.into()), role: Role::Snoozed };
    d.send(Command::Watch { view: snoozed.clone() });
    let row = d.until("the snoozed message in Snoozed", 20, |d| d.list(&snoozed, "", false).into_iter().find(|s| s.subject == "Snooze me"));
    assert_eq!(row.mailbox, bucket, "listed in its bucket");
    d.send(Command::Move { ids: vec![row.id], to: MoveTarget::Inbox, thread: false, undo: None });
    d.until("unsnoozed: back in the inbox on the server", 15, |_| (probe.live("INBOX").iter().any(|s| s == "Snooze me") && probe.live(&bucket).is_empty()).then_some(()));
    println!("✓ unsnoozed from Snoozed: back in the inbox");
}

/// The Envoy that's killed mid-hold, in a process of its own: archives "Sent from Envoy" and
/// sends a message, both held for their undo window, says so, and waits to be killed.
fn hold(pass: Pass) {
    let mut d = Driver::new(Engine::start(|| {}, settings()).expect("engine"));
    let target = d.list(&inbox_view(), "", false).into_iter().find(|s| s.subject == "Sent from Envoy").expect("\"Sent from Envoy\" in the cache");
    let (mark, move_before) = (d.mark(), epoch());
    d.send(Command::Move { ids: vec![target.id], to: MoveTarget::Archive, thread: false, undo: Some(100) });
    d.wait(mark, "the move held", 10, |e| matches!(e, Event::Held { undo: 100, ops: 1 }).then_some(()));
    let (move_after, send_before) = (epoch(), epoch());
    let queued = Draft { account: ACCOUNT.into(), to: pass.user().into(), subject: QUEUED.into(), body: "Still sent.".into(), ..Draft::default() };
    d.send(Command::Send { draft: queued, draft_id: None });
    d.wait(mark, "the send queued", 10, |e| matches!(e, Event::Sent { undo_secs: 5, .. }).then_some(()));
    // When each was issued, for the deadlines' check.
    println!("HELD {move_before} {move_after} {send_before} {}", epoch());
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        d.pump();
    }
}

/// Runs [`hold`] and kills it (SIGKILL: no shutdown, as in a crash or a force quit) once its
/// ops are held. Returns when it issued the move and the send (from, to: [`epoch`]s).
fn kill_mid_hold(pass: Pass) -> [f64; 4] {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap()).args(["hold", pass.name()]).stdout(Stdio::piped()).spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let issued = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if line.starts_with("HELD ") => break line[5..].split(' ').map(|t| t.parse().unwrap()).collect::<Vec<f64>>(),
            Ok(line) => println!("  [killed Envoy] {line}"),
            Err(_) => {
                let _ = child.kill();
                panic!("the Envoy to kill never held its ops");
            }
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();
    issued.try_into().unwrap()
}

/// The engine, and every event it has sent, in order.
struct Driver {
    engine: Engine,
    events: Vec<Event>,
    req: u64,
}

impl Driver {
    fn new(engine: Engine) -> Driver {
        Driver { engine, events: Vec::new(), req: 0 }
    }

    fn send(&self, c: Command) {
        self.engine.send(c);
    }

    /// Takes in what the engine sent. Any error fails the run: none is expected.
    fn pump(&mut self) {
        for ev in self.engine.drain() {
            match &ev {
                Event::Error(e) => panic!("engine error: {e}"),
                Event::AccountFailed(e) => panic!("account failed: {e}"),
                _ => {}
            }
            self.events.push(ev);
        }
    }

    /// Where the events from now on start, for [`Driver::wait`].
    fn mark(&mut self) -> usize {
        self.pump();
        self.events.len()
    }

    /// Waits up to `secs` for an event, from `from` on, that `f` picks.
    fn wait<T>(&mut self, from: usize, what: &str, secs: u64, mut f: impl FnMut(&Event) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut next = from;
        loop {
            self.pump();
            while next < self.events.len() {
                if let Some(t) = f(&self.events[next]) {
                    return t;
                }
                next += 1;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Tries `f` until it gives something, for up to `secs`.
    fn until<T>(&mut self, what: &str, secs: u64, mut f: impl FnMut(&mut Driver) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if let Some(t) = f(self) {
                return t;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn list(&mut self, view: &View, query: &str, conversations: bool) -> Vec<Summary> {
        self.req += 1;
        let (req, from) = (self.req, self.mark());
        self.send(Command::List { req, view: view.clone(), query: query.into(), conversations, limit: 500, stream: true, category: None, counts: false });
        self.wait(from, "list", 10, |e| match e {
            // The whole list, not the first page sent ahead of it.
            Event::List { req: r, items, more: false } if *r == req => Some(items.clone()),
            _ => None,
        })
    }
}

/// Now, in Unix seconds: the clock the engine's deadlines are on.
fn epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

/// The deadline (`not_before`, Unix seconds) the store keeps for the queued ops `which` (an SQL
/// condition on the ops table) picks: the same for each of them.
fn deadline(which: &str) -> i64 {
    let db = rusqlite::Connection::open_with_flags(envoy_core::paths::database_file(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let found: Vec<i64> = db.prepare(&format!("SELECT not_before FROM ops WHERE {which}")).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(!found.is_empty() && found.iter().all(|t| *t == found[0]), "the deadlines of the ops where {which}: {found:?}");
    found[0]
}

/// A held op's deadline is `hold` seconds after it was issued, which was between `before` and
/// `after` ([`epoch`]s).
fn assert_hold(what: &str, deadline: i64, before: f64, after: f64, hold: i64) {
    let range = before.floor() as i64 + hold..=after.floor() as i64 + hold;
    assert!(range.contains(&deadline), "{what}: held until {deadline}, issued between {before:.2} and {after:.2}, for {hold} s");
}

/// A held op to watch on the server ([`watch_holds`]).
struct Hold<'a> {
    what: &'a str,
    deadline: i64,
    /// Any of it is on the server.
    on_server: Box<dyn FnMut() -> bool + 'a>,
}

/// Watches the server through held ops' undo windows: each must be absent from every look that
/// ended before its deadline (one of them less than 2 s before it), and on the server within
/// [`LATE`] after it.
fn watch_holds(d: &mut Driver, mut holds: Vec<Hold>) {
    let mut last_absent = vec![f64::NEG_INFINITY; holds.len()];
    let mut landed = vec![false; holds.len()];
    while landed.contains(&false) {
        d.pump();
        for (i, h) in holds.iter_mut().enumerate() {
            if landed[i] {
                continue;
            }
            let there = (h.on_server)();
            let (at, deadline) = (epoch(), h.deadline as f64);
            if there {
                assert!(at >= deadline, "{}: on the server {:.2} s before its undo window ended", h.what, deadline - at);
                assert!(deadline - last_absent[i] < 2.0, "{}: the server wasn't watched up to its deadline", h.what);
                println!("  {}: on the server {:.1} s after its deadline, and not before", h.what, at - deadline);
                landed[i] = true;
            } else {
                if at < deadline {
                    last_absent[i] = at;
                }
                assert!(at < deadline + LATE, "{}: not on the server {LATE} s after its deadline", h.what);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Watches the server until `until` ([`epoch`]): `untouched` holds on every look. For undone
/// ops, past the deadline they had, when they'd have gone.
fn untouched_until(d: &mut Driver, what: &str, until: f64, mut untouched: impl FnMut() -> bool) {
    loop {
        d.pump();
        assert!(untouched(), "{what}");
        if epoch() > until {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn subjects(rows: &[Summary]) -> Vec<&str> {
    rows.iter().map(|r| r.subject.as_str()).collect()
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort_unstable();
    v
}

/// Tries `f` until it gives something, for up to `secs`.
fn eventually<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn smtp() -> SmtpTransport {
    SmtpTransport::builder_dangerous("localhost").port(SMTP).build()
}

fn deliver(to: &str, subject: &str, from: &str, body: &str, extra: impl FnOnce(lettre::message::MessageBuilder) -> lettre::message::MessageBuilder) {
    let b = Message::builder().from(from.parse().unwrap()).to(to.parse().unwrap()).subject(subject);
    let m = extra(b).body(body.to_owned()).unwrap();
    smtp().send(&m).unwrap();
}

/// A message to APPEND.
fn raw(to: &str, subject: &str, message_id: &str) -> String {
    format!("From: Someone <someone@elsewhere.example>\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: {message_id}\r\nDate: Mon, 2 Nov 2026 09:00:00 +0000\r\n\r\nHello.\r\n")
}

/// A message on the server.
struct ServerMessage {
    subject: String,
    text: String,
    flags: u32,
    raw: Vec<u8>,
}

/// A plain IMAP session straight to GreenMail (never through the proxy), for setting up and for
/// checking what reached the server.
struct Probe {
    rt: tokio::runtime::Runtime,
    user: &'static str,
}

impl Probe {
    fn new(user: &'static str) -> Probe {
        Probe { rt: tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap(), user }
    }

    /// Waits for GreenMail to greet on IMAP and take connections on SMTP.
    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut told = false;
        loop {
            let imap = self.rt.block_on(async {
                let tcp = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(("localhost", IMAP))).await.ok()?.ok()?;
                let mut client = async_imap::Client::new(tcp);
                tokio::time::timeout(Duration::from_secs(5), client.read_response()).await.ok()??.ok()
            });
            if imap.is_some() && std::net::TcpStream::connect(("localhost", SMTP)).is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "GreenMail isn't answering on localhost:{IMAP} (see this file's header to start it)");
            if !told {
                println!("waiting for GreenMail…");
                told = true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn run<T>(&self, f: impl AsyncFnOnce(&mut async_imap::Session<tokio::net::TcpStream>) -> T) -> T {
        self.rt.block_on(async {
            let tcp = tokio::net::TcpStream::connect(("localhost", IMAP)).await.unwrap();
            let mut client = async_imap::Client::new(tcp);
            client.read_response().await.unwrap().unwrap();
            let mut s = client.login(self.user, PASSWORD).await.map_err(|e| e.0).unwrap();
            let out = f(&mut s).await;
            s.logout().await.ok();
            out
        })
    }

    /// Empties every mailbox the user has, and makes the ones in `mailboxes` it hasn't. (Not by
    /// deleting them: GreenMail fails a DELETE of a mailbox a dead session had selected.)
    fn reset(&self, mailboxes: &[&str]) {
        self.run(async move |s| {
            let names: Vec<String> = s.list(Some(""), Some("*")).await.unwrap().map_ok(|n| n.name().to_owned()).try_collect().await.unwrap();
            for name in &names {
                if s.select(name).await.unwrap().exists > 0 {
                    s.store("1:*", "+FLAGS.SILENT (\\Deleted)").await.unwrap().try_collect::<Vec<_>>().await.unwrap();
                    s.expunge().await.unwrap().try_collect::<Vec<_>>().await.unwrap();
                }
            }
            for name in mailboxes.iter().filter(|m| !names.iter().any(|n| n == *m)) {
                s.create(name).await.unwrap();
            }
        })
    }

    fn append(&self, mailbox: &str, flags: &str, raw: &str) {
        self.run(async move |s| s.append(mailbox, Some(flags).filter(|f| !f.is_empty()), None, raw.as_bytes()).await.unwrap())
    }

    fn store(&self, mailbox: &str, uid: u32, query: &str) {
        self.run(async move |s| {
            s.select(mailbox).await.unwrap();
            s.uid_store(uid.to_string(), query).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        })
    }

    /// How many messages the mailbox holds, \Deleted ones included.
    fn count(&self, mailbox: &str) -> u32 {
        self.run(async move |s| s.select(mailbox).await.unwrap().exists)
    }

    fn messages(&self, mailbox: &str) -> Vec<ServerMessage> {
        self.run(async move |s| {
            if s.select(mailbox).await.unwrap().exists == 0 {
                return vec![];
            }
            let fetched: Vec<_> = s.fetch("1:*", "(UID FLAGS BODY.PEEK[])").await.unwrap().try_collect().await.unwrap();
            fetched
                .iter()
                // GreenMail tells every session of flags changed by another (Envoy's), as
                // FETCH responses without a body, in among the answer's.
                .filter_map(|m| {
                    let p = envoy_core::mime::parse(m.body()?, false).expect("a message");
                    Some(ServerMessage { subject: p.subject, text: p.text.unwrap_or_default(), flags: envoy_core::imap::flag_bits(m.flags()), raw: m.body()?.to_vec() })
                })
                .collect()
        })
    }

    /// Subjects of the messages not flagged \Deleted.
    fn live(&self, mailbox: &str) -> Vec<String> {
        self.messages(mailbox).into_iter().filter(|m| m.flags & flags::DELETED == 0).map(|m| m.subject).collect()
    }

    /// Subjects of the messages flagged \Deleted, not expunged.
    fn deleted(&self, mailbox: &str) -> Vec<String> {
        self.messages(mailbox).into_iter().filter(|m| m.flags & flags::DELETED != 0).map(|m| m.subject).collect()
    }

    /// Flags of the message with this subject, if it's there and not flagged \Deleted.
    fn flags(&self, mailbox: &str, subject: &str) -> Option<u32> {
        self.messages(mailbox).into_iter().find(|m| m.subject == subject && m.flags & flags::DELETED == 0).map(|m| m.flags)
    }

    /// The files of the message with this subject, not flagged \Deleted: name and bytes.
    fn files(&self, mailbox: &str, subject: &str) -> Vec<(String, Vec<u8>)> {
        let Some(m) = self.messages(mailbox).into_iter().find(|m| m.subject == subject && m.flags & flags::DELETED == 0) else { return vec![] };
        let d = envoy_core::mime::server_draft(&m.raw).expect("a message");
        d.draft.attachments.into_iter().map(|a| a.filename).zip(d.files).collect()
    }

    /// Every mailbox's name.
    fn names(&self) -> Vec<String> {
        self.run(async move |s| s.list(Some(""), Some("*")).await.unwrap().map_ok(|n| n.name().to_owned()).try_collect().await.unwrap())
    }

    /// The subscribed mailboxes.
    fn subscribed(&self) -> Vec<String> {
        self.run(async move |s| s.lsub(Some(""), Some("*")).await.unwrap().map_ok(|n| n.name().to_owned()).try_collect().await.unwrap())
    }

    /// The server's hierarchy delimiter.
    fn delimiter(&self) -> String {
        self.run(async move |s| {
            let names: Vec<_> = s.list(Some(""), Some("INBOX")).await.unwrap().try_collect().await.unwrap();
            names.first().and_then(|n| n.delimiter().map(str::to_owned)).unwrap_or_else(|| "/".to_owned())
        })
    }

    fn create(&self, mailbox: &str) {
        self.run(async move |s| s.create(mailbox).await.unwrap())
    }

    /// Envoy's drafts in Drafts (subject, text): not flagged \Deleted, other devices' left out.
    fn drafts(&self) -> Vec<(String, String)> {
        let found: Vec<ServerMessage> = self.messages("Drafts").into_iter().filter(|m| m.flags & flags::DELETED == 0).collect();
        assert!(found.iter().any(|m| m.subject == "From my phone"), "the other device's draft was deleted");
        found.into_iter().filter(|m| m.subject != "From my phone").map(|m| (m.subject, m.text)).collect()
    }
}

/// GreenMail as a server without UIDPLUS and MOVE: a TLS proxy to its plain IMAP port that takes
/// them out of what the server says, and refuses (and records) what such a server wouldn't take,
/// and the commands that expunge every message flagged \Deleted.
struct Proxy {
    port: u16,
    /// Every command Envoy sent, without its tag.
    commands: Log,
    /// The ones it shouldn't have.
    violations: Log,
}

type Log = Arc<Mutex<Vec<String>>>;

impl Proxy {
    fn start(dir: &Path) -> Proxy {
        use rustls::pki_types::pem::PemObject;
        let (cert, key) = (dir.join("proxy-cert.pem"), dir.join("proxy-key.pem"));
        let made = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=localhost", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(made.is_ok_and(|s| s.success()), "the plain pass needs openssl, to make its proxy's TLS certificate");
        let cert = rustls::pki_types::CertificateDer::from_pem_slice(&std::fs::read(&cert).unwrap()).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&std::fs::read(&key).unwrap()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (commands, violations): (Log, Log) = Default::default();
        let (all, bad) = (commands.clone(), violations.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                while let Ok((tcp, _)) = listener.accept().await {
                    let (acceptor, all, bad) = (acceptor.clone(), all.clone(), bad.clone());
                    tokio::spawn(async move {
                        let Ok(client) = acceptor.accept(tcp).await else { return };
                        let Ok(server) = tokio::net::TcpStream::connect(("localhost", IMAP)).await else { return };
                        let (client_r, client_w) = tokio::io::split(client);
                        let (server_r, server_w) = server.into_split();
                        // Either side hanging up ends both.
                        tokio::select! {
                            _ = relay(client_r, server_w, |line| refuse(line, &all, &bad)) => {}
                            _ = relay(server_r, client_w, |line| {
                                let out = strip(line);
                                // What goes to Envoy has nothing left to take out.
                                let sent = out.as_deref().unwrap_or(line);
                                if strip(sent).is_some() {
                                    bad.lock().unwrap().push(format!("passed on: {}", String::from_utf8_lossy(sent).trim_end()));
                                }
                                out
                            }) => {}
                        }
                    });
                }
            });
        });
        Proxy { port, commands, violations }
    }
}

/// Copies IMAP from `from` to `to` a line at a time, through `rewrite` (None leaves a line as it
/// is). Literals (`{n}` ending a line, then n bytes) go across untouched.
async fn relay(from: impl tokio::io::AsyncRead + Unpin, mut to: impl tokio::io::AsyncWrite + Unpin, rewrite: impl Fn(&[u8]) -> Option<Vec<u8>>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut from = tokio::io::BufReader::new(from);
    let mut line = Vec::new();
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let out = rewrite(&line);
        if to.write_all(out.as_deref().unwrap_or(&line)).await.is_err() {
            return;
        }
        if let Some(n) = literal(&line) {
            let mut bytes = vec![0; n];
            if from.read_exact(&mut bytes).await.is_err() || to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        if to.flush().await.is_err() {
            return;
        }
    }
}

/// The size of the literal a line ends with: `{12}`, or LITERAL+'s `{12+}`.
fn literal(line: &[u8]) -> Option<usize> {
    let line = line.strip_suffix(b"\n")?;
    let line = line.strip_suffix(b"\r").unwrap_or(line).strip_suffix(b"}")?;
    let open = line.iter().rposition(|b| *b == b'{')?;
    let n = &line[open + 1..];
    std::str::from_utf8(n.strip_suffix(b"+").unwrap_or(n)).ok()?.parse().ok()
}

/// Envoy's commands, as a server without UIDPLUS and MOVE takes them: each is recorded; what that
/// server doesn't have, and EXPUNGE and CLOSE, which would destroy every message flagged \Deleted
/// (other clients' too), are recorded as violations and turned into a command GreenMail refuses.
fn refuse(line: &[u8], all: &Mutex<Vec<String>>, bad: &Mutex<Vec<String>>) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(line).ok()?;
    let (tag, command) = text.trim_end().split_once(' ')?;
    let upper = command.to_ascii_uppercase();
    if !upper.starts_with("LOGIN") {
        all.lock().unwrap().push(command.to_owned());
    }
    if !(["UID EXPUNGE", "UID MOVE ", "MOVE "].iter().any(|c| upper.starts_with(c)) || upper == "EXPUNGE" || upper == "CLOSE") {
        return None;
    }
    eprintln!("  proxy: Envoy sent \"{command}\", which a server without UIDPLUS and MOVE lacks, or which expunges others' mail");
    bad.lock().unwrap().push(command.to_owned());
    Some(format!("{tag} XREFUSED-BY-E2E-PROXY\r\n").into_bytes())
}

/// What GreenMail says, without UIDPLUS and MOVE: out of its capability lists (`* CAPABILITY`,
/// and the `[CAPABILITY ..]` code of a greeting or an OK), and no APPENDUID or COPYUID codes
/// (UIDPLUS's). None leaves the line as it is.
fn strip(line: &[u8]) -> Option<Vec<u8>> {
    let body = std::str::from_utf8(line).ok()?.trim_end_matches(['\r', '\n']);
    let (head, rest) = body.split_once(' ')?;
    let (word, after) = rest.split_once(' ').unwrap_or((rest, ""));
    let out = if head == "*" && word.eq_ignore_ascii_case("CAPABILITY") {
        format!("* {word} {}", without_uidplus_move(after))
    } else if ["OK", "NO", "BAD", "PREAUTH", "BYE"].iter().any(|s| word.eq_ignore_ascii_case(s)) && after.starts_with('[') {
        // A response code: up to the first ']' (none has one inside).
        let end = after.find(']')?;
        let (code, text) = (&after[1..end], &after[end + 1..]);
        let (name, args) = code.split_once(' ').unwrap_or((code, ""));
        if name.eq_ignore_ascii_case("CAPABILITY") {
            format!("{head} {word} [{name} {}]{text}", without_uidplus_move(args))
        } else if name.eq_ignore_ascii_case("APPENDUID") || name.eq_ignore_ascii_case("COPYUID") {
            let text = text.trim_start();
            format!("{head} {word} {}", if text.is_empty() { "done" } else { text })
        } else {
            return None;
        }
    } else {
        return None;
    };
    (out != body).then(|| format!("{out}\r\n").into_bytes())
}

fn without_uidplus_move(capabilities: &str) -> String {
    capabilities.split_ascii_whitespace().filter(|c| !c.eq_ignore_ascii_case("UIDPLUS") && !c.eq_ignore_ascii_case("MOVE")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stripped(line: &str) -> String {
        let out = strip(format!("{line}\r\n").as_bytes()).map_or_else(|| line.to_owned(), |o| String::from_utf8(o).unwrap().trim_end().to_owned());
        assert_eq!(strip(format!("{out}\r\n").as_bytes()), None, "nothing left to take out of {out:?}");
        out
    }

    #[test]
    fn uidplus_and_move_leave_every_capability_list() {
        for (line, want) in [
            ("* CAPABILITY IMAP4rev1 LITERAL+ UIDPLUS SORT IDLE MOVE QUOTA", "* CAPABILITY IMAP4rev1 LITERAL+ SORT IDLE QUOTA"),
            ("* CAPABILITY IMAP4rev1 MOVE", "* CAPABILITY IMAP4rev1"),
            ("* CAPABILITY UIDPLUS IMAP4rev1 move", "* CAPABILITY IMAP4rev1"),
            // A greeting's, and a login's.
            ("* OK [CAPABILITY IMAP4rev1 UIDPLUS MOVE] ready", "* OK [CAPABILITY IMAP4rev1] ready"),
            ("* OK [CAPABILITY MOVE IMAP4rev1 UIDPLUS] GreenMail ready", "* OK [CAPABILITY IMAP4rev1] GreenMail ready"),
            ("* PREAUTH [CAPABILITY IMAP4rev1 IDLE MOVE] logged in", "* PREAUTH [CAPABILITY IMAP4rev1 IDLE] logged in"),
            ("A0001 OK [CAPABILITY IMAP4rev1 IDLE UIDPLUS] LOGIN completed.", "A0001 OK [CAPABILITY IMAP4rev1 IDLE] LOGIN completed."),
            // UIDPLUS's answers.
            ("A0002 OK [APPENDUID 7 21] APPEND completed.", "A0002 OK APPEND completed."),
            ("A0003 OK [COPYUID 7 3 9] COPY completed.", "A0003 OK COPY completed."),
            ("* OK [COPYUID 7 3 9]", "* OK done"),
        ] {
            assert_eq!(stripped(line), want);
        }
    }

    #[test]
    fn everything_else_goes_through_as_it_is() {
        for line in ["* OK [UIDNEXT 7] ok", "* OK [PERMANENTFLAGS (\\Seen \\*)] ok", "A0004 OK MOVE completed [MOVE]", "* 3 FETCH (FLAGS (\\Seen) UID 4)", "* SEARCH 1 2", "* LIST () \".\" \"MOVE\"", "+ idling"] {
            assert_eq!(strip(format!("{line}\r\n").as_bytes()), None, "{line}");
        }
    }

    #[test]
    fn what_that_server_lacks_is_refused() {
        let (all, bad) = (Mutex::default(), Mutex::default());
        for line in ["A1 UID MOVE 3 \"Archive\"", "A2 MOVE 1:2 Archive", "A3 UID EXPUNGE 3", "A4 EXPUNGE", "a5 close"] {
            let out = refuse(format!("{line}\r\n").as_bytes(), &all, &bad).expect(line);
            assert!(String::from_utf8(out).unwrap().ends_with(" XREFUSED-BY-E2E-PROXY\r\n"));
        }
        for line in ["A6 UID COPY 3 \"Archive\"", "A7 UID STORE 3 +FLAGS.SILENT (\\Deleted)", "A8 UID SEARCH UNDELETED HEADER Message-ID \"<x@y>\"", "DONE", "A9 IDLE"] {
            assert_eq!(refuse(format!("{line}\r\n").as_bytes(), &all, &bad), None, "{line}");
        }
        assert_eq!(bad.lock().unwrap().len(), 5);
    }

    #[test]
    fn literals_are_found() {
        assert_eq!(literal(b"* 1 FETCH (UID 4 BODY[] {12}\r\n"), Some(12));
        assert_eq!(literal(b"A1 APPEND \"Drafts\" (\\Draft) {5+}\r\n"), Some(5));
        assert_eq!(literal(b"* 1 FETCH (FLAGS ())\r\n"), None);
        assert_eq!(literal(b"A1 LOGIN {x}\r\n"), None);
    }
}
