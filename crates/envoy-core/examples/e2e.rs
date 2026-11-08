//! End-to-end check of the engine against a local GreenMail server:
//!
//! ```sh
//! docker run -d --rm --name envoy-greenmail \
//!   -e GREENMAIL_OPTS='-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:pw@localhost -Dgreenmail.users.login=email' \
//!   -p 3025:3025 -p 3143:3143 -p 3465:3465 -p 3993:3993 greenmail/standalone:2.1.3
//! cargo run -p envoy-core --example e2e
//! ```

use std::time::{Duration, Instant};

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::engine::{Command, Engine, Event, MoveTarget};
use envoy_core::model::{Draft, Role, View, flags};
use envoy_core::settings::Settings;
use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};
use lettre::{Message, SmtpTransport, Transport};

const USER: &str = "alice@localhost";

fn smtp() -> SmtpTransport {
    SmtpTransport::builder_dangerous("localhost").port(3025).build()
}

fn deliver(subject: &str, from: &str, body: &str, extra: impl FnOnce(lettre::message::MessageBuilder) -> lettre::message::MessageBuilder) {
    let b = Message::builder().from(from.parse().unwrap()).to(USER.parse().unwrap()).subject(subject);
    let m = extra(b).body(body.to_owned()).unwrap();
    smtp().send(&m).unwrap();
}

fn wait<T>(engine: &Engine, what: &str, secs: u64, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for ev in engine.drain() {
            if let Event::Error(e) = &ev {
                println!("  (engine error: {e})");
            }
            if let Event::AccountFailed(e) = &ev {
                panic!("account failed: {e}");
            }
            if let Some(t) = f(&ev) {
                return t;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timed out waiting for {what}");
}

fn list(engine: &Engine, req: u64, view: &View, query: &str, conv: bool) -> Vec<envoy_core::model::Summary> {
    engine.send(Command::List { req, view: view.clone(), query: query.into(), conversations: conv, limit: 500 });
    wait(engine, "list", 10, |e| match e {
        // The whole list, not the first page sent ahead of it.
        Event::List { req: r, items, more: false } if *r == req => Some(items.clone()),
        _ => None,
    })
}

fn main() {
    env_logger::init();
    let home = std::env::temp_dir().join(format!("envoy-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: single-threaded at this point; set before anything reads the environment.
    unsafe {
        std::env::set_var("ENVOY_HOME", &home);
        std::env::set_var("ENVOY_INSECURE_TLS", "1");
        // Keep the test's fake password out of the system keychain.
        std::env::set_var("ENVOY_SECRETS", "file");
    }

    println!("seeding mail…");
    deliver("Plans for Thursday", "Bob <bob@localhost>", "Can we meet Thursday at 2?", |b| b.message_id(Some("<plan-1@localhost>".into())));
    deliver("Re: Plans for Thursday", "Carol <carol@localhost>", "Works for me.\n\nOn Mon, Bob wrote:\n> Can we meet?", |b| {
        b.message_id(Some("<plan-2@localhost>".into())).in_reply_to("<plan-1@localhost>".into()).references("<plan-1@localhost>".into())
    });
    let html = Message::builder()
        .from("News <news@localhost>".parse().unwrap())
        .to(USER.parse().unwrap())
        .subject("Weekly pancake digest")
        .multipart(
            MultiPart::mixed()
                .multipart(MultiPart::alternative_plain_html("Pancakes are great.".to_owned(), "<html><body><h1>Digest</h1><p>Pancakes are <b>great</b>.</p><a href=\"https://example.com\">Read more</a></body></html>".to_owned()))
                .singlepart(Attachment::new("recipe.txt".into()).body(b"flour, eggs, milk".to_vec(), ContentType::TEXT_PLAIN)),
        )
        .unwrap();
    smtp().send(&html).unwrap();
    let _ = SinglePart::plain(String::new());

    let mut settings = Settings::default();
    settings.set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
    settings.set("sync.intervalSeconds", serde_json::json!(15)).unwrap();
    let engine = Engine::start(|| {}, settings).expect("engine");

    let account = Account {
        id: "alice".into(),
        email: USER.into(),
        display_name: "Alice".into(),
        auth: AuthKind::Password,
        username: USER.into(),
        imap_host: "localhost".into(),
        imap_port: 3993,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        hue: 0.0,
        signature: String::new(),
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
    };

    // Folders the test needs (GreenMail starts with INBOX only).
    let probe = Probe::new();
    for name in ["Trash", "Sent", "Archive", "Junk"] {
        probe.create(name);
    }
    for i in 0..3 {
        probe.append("Junk", &format!("From: Spammer <s@spam.example>\r\nTo: alice@localhost\r\nSubject: Win a prize {i}\r\nMessage-ID: <spam-{i}@x>\r\n\r\nClick here\r\n"));
    }

    let t0 = Instant::now();
    engine.send(Command::AddAccount { account: account.clone(), secret: "pw".into() });
    wait(&engine, "account added", 20, |e| matches!(e, Event::AccountAdded(_)).then_some(()));
    println!("✓ account added ({:?})", t0.elapsed());

    let inbox = View::Role { account: Some("alice".into()), role: Role::Inbox };
    engine.send(Command::Watch { view: inbox.clone() });
    let mailboxes = wait(&engine, "mailboxes", 20, |e| match e {
        Event::Mailboxes { mailboxes, .. } if mailboxes.iter().any(|m| m.role == Role::Inbox) => Some(mailboxes.clone()),
        _ => None,
    });
    println!("✓ mailboxes: {:?}", mailboxes.iter().map(|m| format!("{}={:?}", m.name, m.role)).collect::<Vec<_>>());
    assert!(mailboxes.iter().any(|m| m.role == Role::Trash), "Trash found by name");

    // Wait until the headers are in.
    let mut req = 0;
    let deadline = Instant::now() + Duration::from_secs(20);
    let items = loop {
        req += 1;
        let items = list(&engine, req, &inbox, "", false);
        if items.len() >= 3 || Instant::now() > deadline {
            break items;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(items.len(), 3, "three messages synced");
    println!("✓ synced {} messages ({:?} since add)", items.len(), t0.elapsed());

    req += 1;
    let conv = list(&engine, req, &inbox, "", true);
    assert_eq!(conv.len(), 2, "two conversations");
    let plans = conv.iter().find(|s| s.thread_count == 2).expect("a two-message conversation");
    println!("✓ conversations: {} rows, thread participants \"{}\"", conv.len(), plans.participants);

    // Open the HTML message: body arrives (prefetched or on demand), with markdown + attachment.
    let digest = items.iter().find(|s| s.subject.contains("pancake")).unwrap().clone();
    req += 1;
    engine.send(Command::Open { req, id: digest.id, conversations: true });
    let body = wait(&engine, "body", 20, |e| match e {
        Event::Thread { req: r, messages, .. } if *r == req => messages.last().and_then(|m| m.body.clone()),
        _ => None,
    });
    let md = body.markdown.clone().unwrap_or_default();
    assert!(md.contains("**great**"), "markdown: {md}");
    assert_eq!(body.attachments.len(), 1);
    println!("✓ opened HTML message: markdown {:?}…, attachment {}", &md[..md.len().min(40)], body.attachments[0].filename);

    // Search (FTS on body text, after bodies download).
    std::thread::sleep(Duration::from_millis(500));
    req += 1;
    let hits = list(&engine, req, &inbox, "pancakes", false);
    assert!(hits.iter().any(|h| h.id == digest.id), "search finds body text");
    req += 1;
    let hits = list(&engine, req, &inbox, "from:carol", false);
    assert_eq!(hits.len(), 1);
    println!("✓ search: body text and from: filter");

    // Mark read + star, and check the server.
    engine.send(Command::SetFlags { ids: vec![digest.id], add: flags::SEEN | flags::FLAGGED, remove: 0, thread: false });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let fl = probe.flags(digest.uid);
        if fl.iter().any(|x| x.contains("Seen")) && fl.iter().any(|x| x.contains("Flagged")) {
            println!("✓ flags reached the server: {fl:?}");
            break;
        }
        assert!(Instant::now() < deadline, "flags never reached the server: {fl:?}");
        std::thread::sleep(Duration::from_millis(300));
    }

    // Mark as spam, then undo within the window: back in the inbox, and the server never moves it.
    engine.send(Command::Move { ids: vec![digest.id], to: MoveTarget::Junk, thread: false, undo: Some(1) });
    std::thread::sleep(Duration::from_millis(300));
    req += 1;
    assert!(!list(&engine, req, &inbox, "", false).iter().any(|s| s.id == digest.id), "gone from the inbox at once");
    engine.send(Command::UndoMove { undo: 1 });
    std::thread::sleep(Duration::from_millis(300));
    req += 1;
    assert!(list(&engine, req, &inbox, "", false).iter().any(|s| s.id == digest.id), "back after undo");
    std::thread::sleep(Duration::from_secs(7)); // past the 5 s window
    assert_eq!(probe.count("Junk"), 3, "the server never saw the undone move");
    assert_eq!(probe.count("INBOX"), 3);
    println!("✓ spam undone: message back, server untouched");

    // Archive the conversation, undoable: it waits out the window, then reaches the server.
    let t_archive = Instant::now();
    engine.send(Command::Move { ids: vec![plans.id], to: MoveTarget::Archive, thread: true, undo: Some(2) });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(probe.count("Archive"), 0, "held during the undo window");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let n = probe.count("Archive");
        if n == 2 {
            println!("✓ archived conversation: 2 messages in Archive after the window ({:?})", t_archive.elapsed());
            break;
        }
        assert!(Instant::now() < deadline, "archive never reached the server ({n})");
        std::thread::sleep(Duration::from_millis(300));
    }
    req += 1;
    assert_eq!(list(&engine, req, &inbox, "", false).len(), 1, "inbox shows one message locally");
    engine.send(Command::UndoMove { undo: 2 });
    wait(&engine, "too-late toast", 5, |e| matches!(e, Event::Toast(t) if t.starts_with("Too late")).then_some(()));
    println!("✓ undo after the window says it's too late");

    // Opening a folder that isn't synced yet: the foreground connection brings it in at once.
    let junk = View::Mailbox { account: "alice".into(), mailbox: "Junk".into() };
    let t_junk = Instant::now();
    engine.send(Command::Watch { view: junk.clone() });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        req += 1;
        if list(&engine, req, &junk, "", false).len() == 3 {
            println!("✓ opened an unsynced folder: 3 messages in {:?}", t_junk.elapsed());
            break;
        }
        assert!(Instant::now() < deadline, "Junk never synced");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Push: a new message arrives while idle.
    let t_push = Instant::now();
    deliver("Ping while idle", "Dan <dan@localhost>", "IDLE should bring this in.", |b| b);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        req += 1;
        let items = list(&engine, req, &inbox, "", false);
        if items.iter().any(|s| s.subject == "Ping while idle") {
            println!("✓ new mail pushed via IDLE in {:?}", t_push.elapsed());
            break;
        }
        assert!(Instant::now() < deadline, "IDLE push never arrived");
        std::thread::sleep(Duration::from_millis(250));
    }

    // Send through the engine (to ourselves), with an Answered flag on the original.
    let draft = Draft {
        account: "alice".into(),
        to: "Alice <alice@localhost>".into(),
        subject: "Sent from Envoy".into(),
        body: "Hello from the e2e test.".into(),
        reply_of: Some(digest.id),
        ..Draft::default()
    };
    let t_send = Instant::now();
    engine.send(Command::Send { draft });
    wait(&engine, "sent toast", 90, |e| {
        if !matches!(e, Event::List { .. } | Event::Mailboxes { .. } | Event::Changed { .. }) {
            println!("  event: {e:?}");
        }
        matches!(e, Event::Toast(t) if t == "Message sent").then_some(())
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        req += 1;
        let items = list(&engine, req, &inbox, "", false);
        if items.iter().any(|s| s.subject == "Sent from Envoy") {
            println!("✓ sent via SMTP and received back in {:?}", t_send.elapsed());
            break;
        }
        assert!(Instant::now() < deadline, "sent message never arrived");
        std::thread::sleep(Duration::from_millis(250));
    }
    {
        let n = probe.count("Sent");
        assert_eq!(n, 1, "copy saved to Sent (non-Gmail)");
        println!("✓ copy appended to Sent");
    }

    // Delete → Trash.
    engine.send(Command::Move { ids: vec![digest.id], to: MoveTarget::Trash, thread: false, undo: None });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let n = probe.count("Trash");
        if n == 1 {
            println!("✓ deleted to Trash");
            break;
        }
        assert!(Instant::now() < deadline, "trash never reached the server");
        std::thread::sleep(Duration::from_millis(300));
    }

    // Restart: the local cache opens instantly with everything still there.
    drop(engine);
    std::thread::sleep(Duration::from_millis(300));
    let t_cold = Instant::now();
    let engine = Engine::start(|| {}, Settings::default()).unwrap();
    let items = list(&engine, 1, &inbox, "", false);
    println!("✓ restart: {} inbox messages from the cache in {:?} (before any network)", items.len(), t_cold.elapsed());
    assert!(items.len() >= 2);

    println!("\nALL GOOD");
    let _ = std::fs::remove_dir_all(&home);
}

/// A plain IMAP session on GreenMail's 3143 for checking what reached the server.
struct Probe {
    rt: tokio::runtime::Runtime,
}

impl Probe {
    fn new() -> Probe {
        Probe { rt: tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap() }
    }

    fn run<T>(&self, f: impl AsyncFnOnce(&mut async_imap::Session<tokio::net::TcpStream>) -> T) -> T {
        self.rt.block_on(async {
            let tcp = tokio::net::TcpStream::connect(("localhost", 3143)).await.unwrap();
            let mut client = async_imap::Client::new(tcp);
            client.read_response().await.unwrap().unwrap();
            let mut s = client.login(USER, "pw").await.map_err(|e| e.0).unwrap();
            let out = f(&mut s).await;
            s.logout().await.ok();
            out
        })
    }

    fn create(&self, name: &str) {
        let name = name.to_owned();
        self.run(async move |s| {
            let _ = s.create(&name).await;
        })
    }

    fn append(&self, mailbox: &str, raw: &str) {
        let (m, r) = (mailbox.to_owned(), raw.to_owned());
        self.run(async move |s| {
            s.append(&m, None, None, r.as_bytes()).await.unwrap();
        })
    }

    fn count(&self, mailbox: &str) -> u32 {
        let m = mailbox.to_owned();
        self.run(async move |s| s.select(&m).await.unwrap().exists)
    }

    fn flags(&self, uid: u32) -> Vec<String> {
        use futures::TryStreamExt;
        self.run(async move |s| {
            s.select("INBOX").await.unwrap();
            let f: Vec<_> = s.uid_fetch(uid.to_string(), "FLAGS").await.unwrap().try_collect().await.unwrap();
            f.iter().flat_map(|m| m.flags().map(|x| format!("{x:?}")).collect::<Vec<_>>()).collect()
        })
    }
}
