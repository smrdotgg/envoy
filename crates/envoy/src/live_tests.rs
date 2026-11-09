//! The app on the real engine: [`Engine`] on a cache of its own (no account it can reach, so
//! no worker and no server), the app's commands carried out as in the shipped app and its
//! events applied as `poll` applies them. Demo mode makes its lists up from samples on every
//! navigation; these tests say what the app does with a real store behind it.

use std::time::{Duration, Instant};

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::engine::{Command, Engine, MoveTarget};
use envoy_core::model::{Category, MessageId, Role, Summary, View, flags};
use envoy_core::store::Store;
use rune::Tx;

use crate::app::{App, Screen};

/// The account all the cache's mail is in, a Gmail one (labels are Gmail's).
const ACCOUNT: &str = "a";
const ME: &str = "me@x.example";

fn account() -> Account {
    Account {
        id: ACCOUNT.into(),
        email: ME.into(),
        display_name: "Me".into(),
        auth: AuthKind::Password,
        username: String::new(),
        imap_host: "imap.gmail.com".into(),
        imap_port: 993,
        smtp_host: "smtp.gmail.com".into(),
        smtp_port: 465,
        hue: 0.0,
        signature: String::new(),
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
    }
}

/// A message stored in `mailbox` at `uid`, from `from` (`Name <address>`), with flags `fl`
/// (0: unread). Its hour of the day is its UID, so a higher UID is newer.
pub(crate) fn put(st: &Store, mailbox: &str, uid: u32, from: &str, subject: &str, fl: u32) -> MessageId {
    let raw = format!(
        "From: {from}\r\nTo: {ME}\r\nSubject: {subject}\r\nMessage-ID: <{uid}.{}@x.example>\r\nDate: Tue, 3 Mar 2026 {:02}:00:00 +0000\r\n\r\n{subject}: the body.\r\n",
        mailbox.replace('/', "."),
        uid % 24
    );
    let p = envoy_core::mime::parse(raw.as_bytes(), false).expect("parses");
    let id = st.upsert_header(ACCOUNT, mailbox, uid, fl, raw.len() as u32, 0, &p).unwrap();
    st.set_body(id, raw.as_bytes(), &p).unwrap();
    id
}

/// Who's in which tab: rules (they beat Envoy's own judgment), so the tests don't depend on it.
fn rules() -> serde_json::Value {
    serde_json::json!([
        { "from": "maya@northwind.example", "category": "important" },
        { "from": "sam@ortiz.example", "category": "important" },
        { "from": "lee@group.example", "category": "other" },
        { "from": "news@letter.example", "category": "newsletters" },
        { "from": "noreply@github.example", "category": "notifications" },
    ])
}

/// The app, on the inbox, with the engine on a fresh cache named `name` that `seed` filled
/// (its mailboxes made already: INBOX, Archive, Trash, Spam, Travel and Work).
pub(crate) fn live(name: &str, seed: impl FnOnce(&Store)) -> App {
    let mut app = App::new(true); // the default settings: nothing read from this machine
    app.demo = false;
    app.settings.set("inbox.rules", rules()).unwrap();
    let dir = crate::test_home().join(format!("live-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mail.sqlite");
    let mailboxes = {
        let st = Store::open(&db).unwrap();
        // What the engine will sort with (its accounts are the data folder's: none), so it
        // finds nothing to sort again.
        st.set_classifier(envoy_core::category::Classifier::from_settings(&app.settings, Vec::<String>::new())).unwrap();
        for (name, role) in [("INBOX", Role::Inbox), ("[Gmail]/All Mail", Role::All), ("[Gmail]/Trash", Role::Trash), ("[Gmail]/Spam", Role::Junk), ("Travel", Role::Other), ("Work", Role::Other)] {
            st.upsert_mailbox(ACCOUNT, name, name.trim_start_matches("[Gmail]/"), role, "/", true).unwrap();
        }
        seed(&st);
        st.mailboxes(ACCOUNT).unwrap()
    };
    app.engine = Some(Engine::start_at(db, || {}, app.settings.clone()).unwrap());
    app.request_list();
    app.send(Command::Watch { view: app.view.clone() });
    settle(&mut app);
    // The engine has no accounts (it would sync them): the app is told of none, and shows
    // "Add account". It has this one.
    app.accounts = vec![account()];
    app.mailboxes.insert(ACCOUNT.into(), mailboxes);
    app.screen = Screen::Mail;
    app
}

/// Applies the engine's events as they come, as the app's `poll` does, until the engine has
/// been quiet a while with nothing asked for still on its way (a list, an opened conversation).
pub(crate) fn settle(app: &mut App) {
    let mut tx = Tx::default();
    let start = Instant::now();
    let mut quiet = Instant::now();
    loop {
        if app.pump(&mut tx) {
            quiet = Instant::now();
        }
        let waiting = app.list_pending || app.open.is_some_and(|id| app.thread_for != Some(id));
        if !waiting && quiet.elapsed() >= Duration::from_millis(150) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the engine never settled");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn row(app: &App, id: MessageId) -> Option<&Summary> {
    app.list.iter().find(|m| m.id == id)
}

fn ids(app: &App) -> Vec<MessageId> {
    app.list.iter().map(|m| m.id).collect()
}

fn counts(app: &App) -> [u32; 4] {
    Category::ALL.map(|c| app.category_count(c))
}

/// [Important, Other, Newsletters, Notifications] unread.
fn tabs(important: u32, other: u32, newsletters: u32, notifications: u32) -> [u32; 4] {
    let mut out = [0; 4];
    for (c, n) in [(Category::Important, important), (Category::Other, other), (Category::Newsletters, newsletters), (Category::Notifications, notifications)] {
        out[Category::ALL.iter().position(|x| *x == c).unwrap()] = n;
    }
    out
}

fn inbox_seed(st: &Store) -> (MessageId, MessageId, MessageId) {
    let maya = put(st, "INBOX", 5, "Maya Chen <maya@northwind.example>", "Offsite agenda", 0);
    let sam = put(st, "INBOX", 4, "Sam Ortiz <sam@ortiz.example>", "Dinner Friday?", 0);
    let lee = put(st, "INBOX", 3, "Lee Park <lee@group.example>", "Team lunch", 0);
    st.set_counts(ACCOUNT, "INBOX", 3, 3).unwrap();
    (maya, sam, lee)
}

/// Finding: opening an unread conversation, then going to another tab and back, showed it
/// unread again (demo mode makes the list up again from its samples). With the store behind
/// it, read is read: the row and the tab's count say so after any round of tabs.
#[test]
fn read_stays_read_across_tabs() {
    let mut seeded = None;
    let mut app = live("read-tabs", |st| seeded = Some(inbox_seed(st)));
    let (maya, sam, _) = seeded.unwrap();
    assert_eq!(ids(&app), [maya, sam], "the Important tab");
    assert_eq!(counts(&app), tabs(2, 1, 0, 0));
    app.open_message(maya);
    settle(&mut app);
    assert!(!row(&app, maya).unwrap().thread_unread, "read once it shows");
    assert_eq!(counts(&app), tabs(1, 1, 0, 0), "Important's count follows");
    for c in [Category::Other, Category::Important, Category::Newsletters, Category::Important] {
        app.show_category(c);
        settle(&mut app);
    }
    assert!(!row(&app, maya).unwrap().thread_unread, "still read after 2, 1, 3, 1");
    assert!(row(&app, sam).unwrap().thread_unread);
    assert_eq!(counts(&app), tabs(1, 1, 0, 0));
    let inbox = app.mailboxes[ACCOUNT].iter().find(|m| m.role == Role::Inbox).unwrap();
    assert_eq!(inbox.unread, 2, "and the sidebar's");
}

/// Finding: the tabs' unread counts lagged what was done in the list. With the engine, every
/// change made here (read, unread, archive, undo, a move, snooze, a correction and its undo)
/// has the tabs counting as the list does once its answer is in.
#[test]
fn the_tabs_count_what_was_just_done() {
    let mut seeded = None;
    let mut app = live("tab-counts", |st| seeded = Some(inbox_seed(st)));
    let (maya, sam, lee) = seeded.unwrap();
    let mut tx = Tx::default();
    assert_eq!(counts(&app), tabs(2, 1, 0, 0));
    let mut step = |app: &mut App, what: &str, f: &dyn Fn(&mut App, &mut Tx), want: [u32; 4]| {
        f(app, &mut tx);
        settle(app);
        assert_eq!(counts(app), want, "after {what}");
        let unread_rows = app.list.iter().filter(|m| m.thread_unread).count() as u32;
        let tab = app.list_category().unwrap();
        assert_eq!(unread_rows, app.category_count(tab), "after {what}: the list and its tab agree");
    };
    step(&mut app, "reading Maya", &|app, _| app.set_read(vec![maya], true), tabs(1, 1, 0, 0));
    step(&mut app, "marking her unread", &|app, _| app.set_read(vec![maya], false), tabs(2, 1, 0, 0));
    step(&mut app, "archiving her", &|app, tx| app.archive(vec![maya], tx), tabs(1, 1, 0, 0));
    step(&mut app, "undo", &|app, tx| assert!(app.undo_last(tx)), tabs(2, 1, 0, 0));
    step(&mut app, "moving her to Travel", &|app, tx| app.move_to(vec![maya], MoveTarget::Mailbox("Travel".into()), tx), tabs(1, 1, 0, 0));
    step(&mut app, "undo", &|app, tx| assert!(app.undo_last(tx)), tabs(2, 1, 0, 0));
    step(&mut app, "snoozing her", &|app, tx| app.snooze(vec![maya], chrono::Utc::now().timestamp() + 86_400, tx), tabs(1, 1, 0, 0));
    step(&mut app, "undo", &|app, tx| assert!(app.undo_last(tx)), tabs(2, 1, 0, 0));
    step(&mut app, "moving Sam to Other", &|app, tx| assert!(app.move_to_category(vec![sam], Category::Other, tx)), tabs(1, 2, 0, 0));
    step(&mut app, "undo", &|app, tx| assert!(app.undo_last(tx)), tabs(2, 1, 0, 0));
    step(&mut app, "archiving every Important", &|app, tx| app.archive(vec![maya, sam], tx), tabs(0, 1, 0, 0));
    assert!(app.list.is_empty());
    app.show_category(Category::Other);
    settle(&mut app);
    assert_eq!(ids(&app), [lee]);
}

/// Finding: Option+2 moved Maya to Other, and Other didn't have her. A correction is made in
/// the store at once: the tab it went to lists it.
#[test]
fn a_conversation_moved_to_a_tab_is_there() {
    let mut seeded = None;
    let mut app = live("tab-move", |st| seeded = Some(inbox_seed(st)));
    let (maya, _, lee) = seeded.unwrap();
    app.open_message(maya);
    settle(&mut app);
    assert!(app.move_to_category(app.targets(), Category::Other, &mut Tx::default()));
    settle(&mut app);
    assert!(row(&app, maya).is_none(), "off Important");
    app.show_category(Category::Other);
    settle(&mut app);
    assert_eq!(ids(&app), [maya, lee], "in Other, by date");
}

fn mailbox(name: &str) -> View {
    View::Mailbox { account: ACCOUNT.into(), mailbox: name.into() }
}

/// Finding: "Moved to Travel", "Labeled Work", and neither showed the conversation. With the
/// engine too: a move or a label waits out the undo window on this computer before it goes to
/// the server, and the mailbox it goes to had nothing until then (and until it synced). It's
/// there at once now, by date among what's there, and an Undo takes it back out.
#[test]
fn a_moved_or_labeled_conversation_is_at_its_destination_at_once() {
    let mut seeded = None;
    let mut app = live("destinations", |st| {
        seeded = Some(inbox_seed(st));
        put(st, "Travel", 9, "Lufthansa <noreply@lufthansa.example>", "Your booking", flags::SEEN);
        put(st, "Travel", 1, "Rail <noreply@rail.example>", "Your ticket", flags::SEEN);
    });
    let (maya, sam, lee) = seeded.unwrap();
    let mut tx = Tx::default();
    let travel: Vec<MessageId> = {
        app.go(mailbox("Travel"));
        settle(&mut app);
        ids(&app)
    };
    app.go_role(Role::Inbox);
    settle(&mut app);
    app.move_to(vec![maya], MoveTarget::Mailbox("Travel".into()), &mut tx);
    settle(&mut app);
    assert!(row(&app, maya).is_none(), "gone from the inbox");
    app.go(mailbox("Travel"));
    settle(&mut app);
    assert_eq!(ids(&app), [travel[0], maya, travel[1]], "in Travel, by date");
    let r = row(&app, maya).unwrap();
    assert_eq!((r.mailbox.as_str(), r.thread_unread), ("Travel", true), "as it will be there");
    // It opens there, and nothing is done to it until it's there: not marked read, not moved.
    app.open_message(maya);
    settle(&mut app);
    assert_eq!(app.thread.iter().map(|m| m.summary.id).collect::<Vec<_>>(), [maya], "it opens");
    assert!(row(&app, maya).unwrap().thread_unread, "not marked read while on its way");
    let sent = app.sent.borrow().len();
    app.archive(vec![maya], &mut tx);
    app.toggle_star(maya);
    assert!(app.sent.borrow()[sent..].iter().all(|c| !c.starts_with("Move") && !c.starts_with("SetFlags")), "{:?}", &app.sent.borrow()[sent..]);
    assert!(row(&app, maya).is_some_and(|m| !m.flagged()), "still there, unstarred");
    assert!(app.toasts.pop().unwrap().text.starts_with("That's still on the way here"));
    app.toasts.pop();
    // Labeled: a copy in Work, the conversation staying in the inbox.
    app.go_role(Role::Inbox);
    settle(&mut app);
    app.label(vec![sam], "Work".into(), &mut tx);
    settle(&mut app);
    assert!(row(&app, sam).is_some(), "a label leaves it where it is");
    app.go(mailbox("Work"));
    settle(&mut app);
    assert_eq!(ids(&app), [sam], "and it's in Work");
    // Archived and deleted: in All Mail and in Trash.
    app.go_role(Role::Inbox);
    settle(&mut app);
    app.show_category(Category::Other);
    settle(&mut app);
    app.delete_now(vec![lee], &mut tx);
    settle(&mut app);
    app.go_role(Role::Trash);
    settle(&mut app);
    assert_eq!(ids(&app), [lee], "in Trash");
    // Undone, each leaves where it went.
    for _ in 0..3 {
        assert!(app.undo_last(&mut tx));
    }
    settle(&mut app);
    assert!(app.list.is_empty(), "Trash: {:?}", ids(&app));
    for (view, want) in [(mailbox("Work"), vec![]), (mailbox("Travel"), travel.clone())] {
        app.go(view.clone());
        settle(&mut app);
        assert_eq!(ids(&app), want, "{view:?} after the undos");
    }
}
