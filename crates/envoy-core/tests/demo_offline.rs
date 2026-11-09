//! Demo mode's engine: the real one, against the in-process server (`envoy_core::demo`), and
//! never a connection. A test binary of its own, so the process's sockets are its alone to count.

use std::sync::Arc;
use std::time::{Duration, Instant};

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::demo::{Mail, Server};
use envoy_core::engine::{Command, Engine, Event, MoveTarget};
use envoy_core::model::{Draft, Role, Summary, View, flags};
use envoy_core::settings::Settings;

fn account() -> Account {
    Account {
        id: "demo".into(),
        email: "you@gmail.com".into(),
        display_name: "You".into(),
        auth: AuthKind::Password,
        username: "you@gmail.com".into(),
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

fn mail(n: u32, subject: &str) -> Vec<u8> {
    Mail {
        from: ("Maya Chen".into(), "maya@northwind.dev".into()),
        to: vec![("You".into(), "you@gmail.com".into())],
        subject: subject.into(),
        date: 1_780_000_000 + i64::from(n) * 60,
        message_id: format!("m{n}@demo.test"),
        text: Some(format!("Message {n}")),
        attachment: (n == 2).then(|| ("plan.pdf".into(), "application/pdf".into(), 2_000)),
        ..Mail::default()
    }
    .rfc822()
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

struct Demo {
    engine: Engine,
    server: Arc<Server>,
    req: u64,
}

impl Demo {
    /// Waits for an event `want` picks something out of, up to a few seconds.
    fn wait_for<T>(&self, mut want: impl FnMut(&Event) -> Option<T>) -> T {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            for e in self.engine.wait(Duration::from_millis(20)) {
                if let Some(t) = want(&e) {
                    return t;
                }
            }
        }
        panic!("the engine never said it");
    }

    fn list(&mut self, view: View) -> Vec<Summary> {
        self.req += 1;
        let req = self.req;
        self.engine.send(Command::List { req, view, query: String::new(), conversations: false, limit: 100, stream: false, category: None, counts: false });
        self.wait_for(|e| match e {
            Event::List { req: r, items, more: false } if *r == req => Some(items.clone()),
            _ => None,
        })
    }

    fn until_server(&self, check: impl Fn(&Server) -> bool) {
        let started = Instant::now();
        while !check(&self.server) {
            assert!(started.elapsed() < Duration::from_secs(10), "the server never got there");
            let _ = self.engine.wait(Duration::from_millis(20));
        }
    }
}

#[test]
fn demo_mode_runs_the_real_engine_and_never_connects() {
    let _ = env_logger::builder().is_test(true).try_init();
    let home = std::env::temp_dir().join(format!("envoy-demo-offline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    assert!(envoy_core::paths::use_home(home.clone()));
    // The check below would see a connection: a listening socket counts too.
    if cfg!(target_os = "linux") {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert_eq!(network_sockets(), 1, "the check sees sockets");
        drop(listener);
    }
    assert_eq!(network_sockets(), 0);
    envoy_core::net::go_offline();

    let server = Arc::new(Server::new());
    server.add_account(account(), &[("[Gmail]/Sent Mail", "\\Sent"), ("[Gmail]/All Mail", "\\All"), ("[Gmail]/Drafts", "\\Drafts"), ("[Gmail]/Trash", "\\Trash"), ("Snoozed", ""), ("Travel", ""), ("INBOX", "")]);
    for n in 1..=3 {
        server.add_mail("demo", &["INBOX"], mail(n, &format!("Subject {n}")), if n == 1 { 0 } else { flags::SEEN }, 1_780_000_000 + i64::from(n) * 60);
    }
    let mut settings = Settings::default();
    settings.set("behavior.undoSeconds", serde_json::json!(0)).unwrap();
    settings.set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
    let started = Instant::now();
    let engine = Engine::start_offline(|| {}, settings, server.clone(), home.join("mail.sqlite")).unwrap();
    let mut demo = Demo { engine, server, req: 0 };

    // Filled before the first list is answered: headers, bodies, counts, as a first sync would.
    let inbox = demo.list(View::Role { account: None, role: Role::Inbox });
    eprintln!("first list in {:?}", started.elapsed());
    assert_eq!(inbox.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(), ["Subject 3", "Subject 2", "Subject 1"]);
    assert!(inbox.iter().find(|m| m.subject == "Subject 2").unwrap().has_attachments, "its body's in: it has its file");
    let id = |list: &[Summary], s: &str| list.iter().find(|m| m.subject == s).unwrap().id;

    // A flag, a move and a send, each through the op queue to the server.
    demo.engine.send(Command::SetFlags { ids: vec![id(&inbox, "Subject 1")], add: flags::SEEN, remove: 0, thread: false });
    demo.until_server(|s| s.contents("demo", "INBOX").iter().any(|(f, m)| m == "<m1@demo.test>" && f & flags::SEEN != 0));
    demo.engine.send(Command::Move { ids: vec![id(&inbox, "Subject 2")], to: MoveTarget::Mailbox("Travel".into()), thread: false, undo: None });
    demo.until_server(|s| s.contents("demo", "Travel").iter().any(|(_, m)| m == "<m2@demo.test>") && !s.contents("demo", "INBOX").iter().any(|(_, m)| m == "<m2@demo.test>"));
    demo.engine.send(Command::Watch { view: View::Mailbox { account: "demo".into(), mailbox: "Travel".into() } });
    let started = Instant::now();
    loop {
        let travel = demo.list(View::Mailbox { account: "demo".into(), mailbox: "Travel".into() });
        if travel.iter().any(|m| m.subject == "Subject 2") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "Travel synced with it in");
    }
    let draft = Draft { account: "demo".into(), to: "sam@ortiz.family".into(), subject: "Tacos".into(), body: "Count me in".into(), ..Draft::default() };
    demo.engine.send(Command::Send { draft, draft_id: None });
    demo.wait_for(|e| matches!(e, Event::Toast(t) if t.contains("nothing left your computer")).then_some(()));
    assert_eq!(demo.server.contents("demo", "[Gmail]/Sent Mail").len(), 1, "filed in Sent, as Gmail does");

    // What only the network could do is refused, without trying.
    demo.engine.send(Command::AddAccount { account: Account { id: "other".into(), email: "me@example.com".into(), imap_host: "imap.example.com".into(), ..account() }, secret: "pw".into() });
    demo.wait_for(|e| matches!(e, Event::AccountFailed(why) if why.contains("Demo mode")).then_some(()));
    demo.engine.send(Command::FetchImage { url: "https://img.example/a.png".into(), message: 1 });
    demo.wait_for(|e| matches!(e, Event::ImageFailed { .. }).then_some(()));
    demo.engine.send(Command::SyncNow);
    std::thread::sleep(Duration::from_millis(300));
    let _ = demo.engine.drain();

    assert_eq!(envoy_core::net::refused(), 0, "nothing tried to connect");
    assert_eq!(network_sockets(), 0, "and no connection is open");
    drop(demo);
    let _ = std::fs::remove_dir_all(&home);
}
