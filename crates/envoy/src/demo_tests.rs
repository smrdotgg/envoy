//! Demo mode as a user meets it: the real engine and store against the demo's in-process server.
//! What's done lasts, in the store and on that server, whichever view is shown after, and nothing
//! ever opens a connection.

use rune::{TextEdit, Tx};
use serde_json::json;

use envoy_core::engine::{Command, MoveTarget};
use envoy_core::model::{Category, MessageId, Role, Summary, View, flags};

use crate::app::{App, Undo};
use crate::demo::{self, ACCOUNT};

const INBOX: &str = "INBOX";
const SENT: &str = "[Gmail]/Sent Mail";

/// Demo mode, its inbox shown.
fn demo() -> App {
    demo::testing::app()
}

/// Actions reach the server at once: no undo window, sends included.
fn at_once(app: &mut App) {
    app.set_setting("behavior.undoSeconds", json!(0));
    app.set_setting("compose.undoSendSeconds", json!(0));
    app.settle();
}

fn go(app: &mut App, view: View) {
    app.go(view);
    app.settle();
}

fn go_role(app: &mut App, role: Role) {
    go(app, View::Role { account: Some(ACCOUNT.into()), role });
}

fn folder(name: &str) -> View {
    View::Mailbox { account: ACCOUNT.into(), mailbox: name.into() }
}

fn row<'a>(app: &'a App, subject: &str) -> Option<&'a Summary> {
    app.list.iter().find(|m| m.subject == subject)
}

fn id_of(app: &App, subject: &str) -> MessageId {
    row(app, subject).unwrap_or_else(|| panic!("no {subject:?} in {:?}", subjects(app))).id
}

fn subjects(app: &App) -> Vec<String> {
    app.list.iter().map(|m| m.subject.clone()).collect()
}

/// Waits until the list on screen shows (or stops showing) `subject`.
fn shows(app: &mut App, subject: &str, wanted: bool) -> bool {
    app.settle_until(|app| row(app, subject).is_some() == wanted)
}

/// The Message-IDs in a folder on the demo's server.
fn on_server(app: &App, folder: &str) -> Vec<String> {
    app.demo_server.as_ref().expect("demo mode's server").contents(ACCOUNT, folder).into_iter().map(|(_, id)| id).collect()
}

/// Waits until `check` holds of the demo's server.
fn server_until(app: &mut App, check: impl Fn(&envoy_core::demo::Server) -> bool) -> bool {
    let server = app.demo_server.clone().expect("demo mode's server");
    app.settle_until(move |_| check(&server))
}

fn inbox_unread(app: &App) -> u32 {
    app.mailboxes[ACCOUNT].iter().find(|m| m.role == Role::Inbox).map_or(0, |m| m.unread)
}

#[test]
fn moves_and_labels_last_and_show_where_they_went() {
    let mut app = demo();
    at_once(&mut app);
    let mut tx = Tx::default();
    // Archived: off the inbox, on the server too, and in All Mail when it's opened.
    let sam = id_of(&app, "Dinner Saturday?");
    app.archive(vec![sam], &mut tx);
    assert!(row(&app, "Dinner Saturday?").is_none(), "off the list at once");
    assert!(server_until(&mut app, |s| !s.contents(ACCOUNT, INBOX).iter().any(|(_, m)| m.starts_with("<s3."))), "archived on the server");
    go_role(&mut app, Role::All);
    assert!(shows(&mut app, "Dinner Saturday?", true), "in All Mail: {:?}", subjects(&app));
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "Dinner Saturday?").is_none(), "and not back in the inbox: {:?}", subjects(&app));
    // Moved to a folder: there, and only there.
    let alex = id_of(&app, "photos from the trip!!");
    app.move_to(vec![alex], MoveTarget::Mailbox("Travel".into()), &mut tx);
    go(&mut app, folder("Travel"));
    assert!(shows(&mut app, "photos from the trip!!", true), "in Travel: {:?}", subjects(&app));
    assert!(row(&app, "Your booking confirmation: SFO → MUC").is_some(), "beside what was there");
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "photos from the trip!!").is_none(), "{:?}", subjects(&app));
    // Labeled: in the label, and still in the inbox.
    let priya = id_of(&app, "Re: Q3 roadmap draft");
    app.label(vec![priya], "Receipts".into(), &mut tx);
    assert!(server_until(&mut app, |s| s.contents(ACCOUNT, "Receipts").iter().any(|(_, m)| m.starts_with("<s5."))), "labeled on the server");
    go(&mut app, folder("Receipts"));
    assert!(shows(&mut app, "Re: Q3 roadmap draft", true), "in Receipts: {:?}", subjects(&app));
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "Re: Q3 roadmap draft").is_some(), "still in the inbox: {:?}", subjects(&app));
    // Moved to another tab: there, after going elsewhere and back.
    assert!(app.show_category(Category::Notifications));
    app.settle();
    let github = id_of(&app, "[envoy] Pull request #42: Faster thread loading");
    app.move_to_category(vec![github], Category::Important, &mut tx);
    go_role(&mut app, Role::Sent);
    go_role(&mut app, Role::Inbox);
    app.show_category(Category::Important);
    app.settle();
    assert!(row(&app, "[envoy] Pull request #42: Faster thread loading").is_some(), "on the Important tab: {:?}", subjects(&app));
    assert_eq!(app.list_category(), Some(Category::Important));
    // Deleted: in Trash, out of the inbox.
    let maya = id_of(&app, "Design review moved to Thursday");
    app.delete_now(vec![maya], &mut tx);
    go_role(&mut app, Role::Trash);
    assert!(shows(&mut app, "Design review moved to Thursday", true), "in Trash: {:?}", subjects(&app));
}

#[test]
fn undo_keeps_a_move_off_the_server_and_one_not_undone_gets_there() {
    let mut app = demo();
    app.set_setting("behavior.undoSeconds", json!(1));
    let mut tx = Tx::default();
    let maya = id_of(&app, "Design review moved to Thursday");
    app.archive(vec![maya], &mut tx);
    assert!(matches!(app.toasts.last().and_then(|t| t.undo.as_ref()), Some(Undo::Move(_))), "with Undo");
    assert!(app.undo_last(&mut tx));
    assert!(row(&app, "Design review moved to Thursday").is_some(), "back at once");
    // Past the window: the server never heard of it, and the inbox, read again, has it.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    app.settle();
    assert!(on_server(&app, INBOX).iter().any(|m| m == "<s0.2@demo.envoy>"), "untouched: {:?}", on_server(&app, INBOX));
    go_role(&mut app, Role::Sent);
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "Design review moved to Thursday").is_some(), "{:?}", subjects(&app));
    // Not undone: it reaches the server once the window is over.
    app.archive(vec![maya], &mut tx);
    assert!(server_until(&mut app, |s| !s.contents(ACCOUNT, INBOX).iter().any(|(_, m)| m == "<s0.2@demo.envoy>")), "archived on the server");
    go_role(&mut app, Role::Sent);
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "Design review moved to Thursday").is_none(), "{:?}", subjects(&app));
}

#[test]
fn a_snooze_is_in_snoozed_until_it_wakes_atop_the_inbox() {
    let mut app = demo();
    at_once(&mut app);
    let mut tx = Tx::default();
    let now = chrono::Local::now();
    let tomorrow = envoy_core::snooze::presets(&now).into_iter().find(|p| p.id == "tomorrow").expect("tomorrow").at;
    let maya = id_of(&app, "Design review moved to Thursday");
    app.snooze(vec![maya], tomorrow, &mut tx);
    assert!(row(&app, "Design review moved to Thursday").is_none(), "off the inbox at once");
    let bucket = envoy_core::snooze::bucket(envoy_core::snooze::PARENT, "/", tomorrow);
    assert!(server_until(&mut app, |s| s.contents(ACCOUNT, &bucket).iter().any(|(_, m)| m == "<s0.2@demo.envoy>")), "in its bucket on the server");
    go_role(&mut app, Role::Snoozed);
    assert!(shows(&mut app, "Design review moved to Thursday", true), "in Snoozed: {:?}", subjects(&app));
    let r = row(&app, "Design review moved to Thursday").unwrap();
    assert_eq!(app.wake_of(&r.account, &r.mailbox).map(|at| at / 60), Some(tomorrow / 60), "until when it was snoozed");
    assert!(row(&app, "Your hold is ready for pickup").is_some(), "beside the samples' snoozes");
    go_role(&mut app, Role::Inbox);
    assert!(row(&app, "Design review moved to Thursday").is_none(), "{:?}", subjects(&app));
    // One whose time came while Envoy was closed wakes on the next check: atop the inbox,
    // unread, marked, and marked on the server too.
    demo::snoozed_till(app.demo_server.as_ref().unwrap(), now.timestamp(), 600);
    app.send(Command::SyncNow);
    assert!(app.settle_until(|app| app.list.first().is_some_and(|m| m.subject == "Can you send the slides before Friday?")), "woken: {:?}", subjects(&app));
    let woken = app.list[0].clone();
    assert!(woken.flags & flags::WOKEN != 0 && woken.thread_unread, "{woken:?}");
    assert!(app.demo_server.as_ref().unwrap().contents(ACCOUNT, INBOX).iter().any(|(f, m)| m == "<woken.1@demo.envoy>" && f & flags::WOKEN != 0));
    // Read, and left: dealt with, on the server too.
    app.open_message(woken.id);
    app.settle();
    go_role(&mut app, Role::Sent);
    assert!(server_until(&mut app, |s| s.contents(ACCOUNT, INBOX).iter().any(|(f, m)| m == "<woken.1@demo.envoy>" && f & flags::WOKEN == 0 && f & flags::SEEN != 0)));
}

#[test]
fn read_state_stars_and_counts_agree_whatever_is_shown() {
    let mut app = demo();
    // Two unread conversations on the Important tab (Maya's, Sam's), GitHub's on Notifications:
    // three unread messages in the inbox.
    assert_eq!((app.category_count(Category::Important), app.category_count(Category::Notifications), inbox_unread(&app)), (2, 1, 3));
    let maya = id_of(&app, "Design review moved to Thursday");
    assert!(row(&app, "Design review moved to Thursday").unwrap().thread_unread);
    app.open_message(maya);
    app.settle();
    assert!(!row(&app, "Design review moved to Thursday").unwrap().thread_unread, "read once open");
    for role in [Role::Sent, Role::Snoozed, Role::Inbox] {
        go_role(&mut app, role);
    }
    assert!(!row(&app, "Design review moved to Thursday").unwrap().thread_unread, "still read after going elsewhere");
    assert_eq!((app.category_count(Category::Important), inbox_unread(&app)), (1, 2), "the tab and the sidebar count it read");
    assert!(server_until(&mut app, |s| s.contents(ACCOUNT, INBOX).iter().any(|(f, m)| m == "<s0.2@demo.envoy>" && f & flags::SEEN != 0)), "read on the server");
    // Unread again, and starred: both last.
    app.set_read(vec![maya], false);
    app.toggle_star(maya);
    app.settle();
    go_role(&mut app, Role::Sent);
    go_role(&mut app, Role::Inbox);
    let r = row(&app, "Design review moved to Thursday").unwrap();
    assert!(r.thread_unread && r.flagged(), "{r:?}");
    // Every message of it unread again: both of Maya's in the inbox.
    assert_eq!((app.category_count(Category::Important), inbox_unread(&app)), (2, 4));
    // Starred is Gmail's Starred: the starred mail of every folder.
    app.go_role(Role::Flagged);
    app.settle();
    assert!(shows(&mut app, "Design review moved to Thursday", true), "in Starred: {:?}", subjects(&app));
    assert!(row(&app, "Your booking confirmation: SFO → MUC").is_some() && row(&app, "Re: Q3 roadmap draft").is_some(), "{:?}", subjects(&app));
}

#[test]
fn sending_files_a_copy_in_sent_and_mail_to_yourself_arrives() {
    let mut app = demo();
    let menu = crate::test_home().join("demo-send-menu.pdf");
    std::fs::write(&menu, b"%PDF tacos").unwrap();
    // Undone within its window: back in the composer, and nothing sent.
    app.compose_new();
    let c = app.compose.as_mut().unwrap();
    (c.to, c.subject, c.body) = (TextEdit::new("Sam Ortiz <sam@ortiz.family>"), TextEdit::new("Tacos"), TextEdit::new("Count me in!"));
    app.attach_paths(vec![menu]);
    assert!(app.settle_until(|app| app.compose.as_ref().is_some_and(|c| c.files.iter().all(|f| f.status == envoy_core::model::FileStatus::Attached))));
    let sent_before = on_server(&app, SENT).len();
    app.send_compose();
    assert!(app.settle_until(|app| app.toasts.iter().any(|t| matches!(t.undo, Some(Undo::Send(_))))), "Sending…, with Undo");
    assert!(app.undo_last(&mut Tx::default()));
    assert!(app.settle_until(|app| app.compose.as_ref().is_some_and(|c| c.subject.text == "Tacos" && c.files.len() == 1)), "back in the composer, file and all");
    assert_eq!(on_server(&app, SENT).len(), sent_before, "nothing sent");
    // Sent: the demo's "server" files it in Sent, as Gmail does, and the app says nothing left.
    at_once(&mut app);
    app.send_compose();
    assert!(app.settle_until(|app| app.toasts.iter().any(|t| t.text == "Sent (demo mode: nothing left your computer)")), "{:?}", app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
    assert_eq!(on_server(&app, SENT).len(), sent_before + 1);
    go_role(&mut app, Role::Sent);
    assert!(shows(&mut app, "Tacos", true), "in Sent: {:?}", subjects(&app));
    assert!(app.settle_until(|app| row(app, "Tacos").is_some_and(|r| r.has_attachments)), "with its file, once its body's in");
    go_role(&mut app, Role::Drafts);
    assert!(row(&app, "Tacos").is_none(), "and its draft gone: {:?}", subjects(&app));
    // To yourself: it comes back, to the inbox.
    app.compose_new();
    let c = app.compose.as_mut().unwrap();
    (c.to, c.subject, c.body) = (TextEdit::new("you@gmail.com"), TextEdit::new("Note to self"), TextEdit::new("Buy limes."));
    app.send_compose();
    go_role(&mut app, Role::Inbox);
    assert!(shows(&mut app, "Note to self", true), "in the inbox: {:?}", subjects(&app));
    assert!(row(&app, "Note to self").unwrap().thread_unread);
}

#[test]
fn a_draft_saved_in_demo_mode_reaches_its_drafts_folder() {
    let mut app = demo();
    app.compose_new();
    let c = app.compose.as_mut().unwrap();
    (c.to, c.subject, c.body) = (TextEdit::new("Priya Patel <priya@acme.co>"), TextEdit::new("Roadmap notes"), TextEdit::new("A few thoughts on the sync milestone."));
    app.compose_edited();
    // Put away for another: saved here, and uploaded to Drafts at once.
    app.compose_new();
    app.compose = None;
    assert!(server_until(&mut app, |s| s.contents(ACCOUNT, demo::DRAFTS).len() == 2), "uploaded beside the sample draft");
    go(&mut app, folder(demo::DRAFTS));
    assert!(app.settle_until(|app| app.list.len() == 2 && row(app, "Roadmap notes").is_some()), "one row for it: {:?}", subjects(&app));
}

#[test]
fn what_only_the_network_could_do_is_refused_and_nothing_connects() {
    envoy_core::net::go_offline();
    let refused = envoy_core::net::refused();
    let mut app = demo();
    at_once(&mut app);
    let mut tx = Tx::default();
    // A busy session: reading, archiving, labeling, snoozing, sending, checking for mail.
    let maya = id_of(&app, "Design review moved to Thursday");
    app.open_message(maya);
    app.settle();
    app.compose_reply(None, false, true);
    app.settle();
    app.compose = None;
    let sam = id_of(&app, "Dinner Saturday?");
    app.label(vec![sam], "Work".into(), &mut tx);
    app.archive(vec![sam], &mut tx);
    app.snooze(vec![id_of(&app, "photos from the trip!!")], chrono::Utc::now().timestamp() + 7200, &mut tx);
    app.compose_new();
    let c = app.compose.as_mut().unwrap();
    (c.to, c.subject) = (TextEdit::new("sam@ortiz.family"), TextEdit::new("Hi"));
    app.send_compose();
    app.send(Command::SyncNow);
    app.settle();
    // A picture from the web isn't fetched; adding an account is refused, saying why.
    app.send(Command::FetchImage { url: "https://img.example/a.png".into(), message: maya });
    app.add.email = TextEdit::new("me@example.com");
    app.add.password = TextEdit::new("secret");
    app.add.mode = crate::app::AddMode::Imap;
    app.add.imap_host = TextEdit::new("imap.example.com");
    app.add.smtp_host = TextEdit::new("smtp.example.com");
    app.submit_add();
    assert!(app.settle_until(|app| app.add.error.as_deref().is_some_and(|e| e.contains("Demo mode"))), "{:?}", app.add.error);
    assert!(app.settle_until(|app| matches!(app.images.get("https://img.example/a.png"), Some(crate::app::ImageState::Failed))));
    assert_eq!(envoy_core::net::refused(), refused, "nothing even tried to connect");
    assert_eq!(network_sockets(), 0, "no connection open");
}

/// How many network connections (TCP or UDP, IPv4 or IPv6) the process has open, where that can
/// be told (Linux): its sockets, found in the kernel's tables of network sockets.
fn network_sockets() -> usize {
    let Ok(fds) = std::fs::read_dir("/proc/self/fd") else { return 0 };
    let mine: Vec<String> = fds
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter_map(|l| l.to_string_lossy().strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')).map(str::to_owned))
        .collect();
    ["tcp", "tcp6", "udp", "udp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(format!("/proc/self/net/{t}")).ok())
        .flat_map(|table| table.lines().skip(1).filter_map(|l| l.split_whitespace().nth(9).map(str::to_owned)).collect::<Vec<_>>())
        .filter(|inode| mine.contains(inode))
        .count()
}


#[test]
fn the_rows_the_ui_scripts_tap_are_the_demos() {
    // scripts/ui tap list rows by key, `m` and the message's id in the store, as the demo fills
    // it: Maya's conversation is m9 (the inbox's first), Sam's m13, the sample draft m7.
    let mut app = demo::testing::with(|app| drop(app.settings.set("inbox.split", json!(false))));
    let mut rows: Vec<(String, String)> = app.list.iter().map(|m| (format!("m{}", m.id), m.subject.clone())).collect();
    go(&mut app, folder(demo::DRAFTS));
    rows.extend(app.list.iter().map(|m| (format!("m{}", m.id), m.subject.clone())));
    let subject = |key: &str| rows.iter().find(|(k, _)| k == key).map(|(_, s)| s.as_str());
    assert_eq!(subject("m9"), Some("Design review moved to Thursday"));
    assert_eq!(subject("m13"), Some("Dinner Saturday?"));
    assert_eq!(subject("m7"), Some("Offsite travel plan"));
    let scripts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/ui");
    for script in std::fs::read_dir(scripts).unwrap().flatten() {
        let text = std::fs::read_to_string(script.path()).unwrap();
        let events: serde_json::Value = serde_json::from_str(&text).unwrap();
        for e in events["events"].as_array().unwrap() {
            if let Some(key) = e["key"].as_str().filter(|k| k.len() > 1 && k.starts_with('m') && k[1..].chars().all(|c| c.is_ascii_digit())) {
                assert!(subject(key).is_some(), "{} taps {key}, no row of the demo's", script.path().display());
            }
        }
    }
}

