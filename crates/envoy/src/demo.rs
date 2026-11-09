//! Demo mode (`--demo`, `ENVOY_DEMO=1`): the real app, engine and store, on sample mail kept by
//! an IMAP server in this process (`envoy_core::demo`). The engine fills its cache from it as a
//! first sync would, and every action goes the way it does with a real account: queued, sent to
//! that server, synced back. Nothing leaves the computer: no connection is ever opened
//! (`envoy_core::net`), and the demo's files (its cache, settings, drafts kept at close) go in a
//! folder of its own, deleted as it closes.

use std::sync::Arc;

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::demo::{Mail, Server};
use envoy_core::engine::Engine;
use envoy_core::model::flags;

use crate::app::App;

/// The demo account's id.
pub const ACCOUNT: &str = "demo";
const ME: (&str, &str) = ("You", "you@gmail.com");

pub fn account() -> Account {
    Account {
        id: ACCOUNT.into(),
        email: ME.1.into(),
        display_name: ME.0.into(),
        auth: AuthKind::Password,
        username: ME.1.into(),
        // Gmail's, so it's a Gmail account (labels, snoozes as labels): its server is the demo's.
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

/// The account's folders, as Gmail has them. Listed in the order they're first synced: Sent
/// first, so the people you've written to are known as the inbox is sorted, and the inbox last,
/// so a conversation's newest copy (Gmail keeps one in each of its labels) is the inbox's.
const FOLDERS: &[(&str, &str)] = &[
    ("[Gmail]/Sent Mail", "\\Sent"),
    ("[Gmail]/Starred", "\\Flagged"),
    ("[Gmail]/Important", "\\Important"),
    ("[Gmail]/Drafts", "\\Drafts"),
    ("[Gmail]/All Mail", "\\All"),
    ("[Gmail]/Spam", "\\Junk"),
    ("[Gmail]/Trash", "\\Trash"),
    ("Snoozed", ""),
    ("Receipts", ""),
    ("Travel", ""),
    ("Work", ""),
    ("INBOX", ""),
];

pub const DRAFTS: &str = "[Gmail]/Drafts";

/// Who's who in the sample mail.
const MAYA: (&str, &str) = ("Maya Chen", "maya@northwind.dev");
const JORDAN: (&str, &str) = ("Jordan Lee", "jordan@northwind.dev");
const SAM: (&str, &str) = ("Sam Ortiz", "sam@ortiz.family");
const PRIYA: (&str, &str) = ("Priya Patel", "priya@acme.co");
const ALEX: (&str, &str) = ("Alex Kim", "alex.kim@gmail.com");
const NORA: (&str, &str) = ("Nora Blake", "nora@blake.studio");

/// What kind of sender: what its mail's headers say.
#[derive(Clone, Copy)]
enum Kind {
    /// A person, writing to you.
    Person,
    /// A person writing to a group you're copied on.
    Group,
    /// A newsletter: unsubscribe link, list.
    Newsletter(&'static str),
    /// GitHub's notifications.
    GitHub,
    /// Sent by a machine (`Auto-Submitted`).
    Automated,
    /// A receipt or booking, by its sender or subject.
    Receipt,
}

struct Sample {
    from: (&'static str, &'static str),
    kind: Kind,
    subject: &'static str,
    /// The newest message's first paragraph.
    text: &'static str,
    hours_ago: i64,
    unread: bool,
    starred: bool,
    /// Who wrote the messages before the newest, oldest first: true for you (in Sent).
    earlier: &'static [bool],
    attachment: Option<(&'static str, &'static str, usize)>,
    /// Where the conversation is, besides All Mail.
    folders: &'static [&'static str],
}

const INBOX: &[&str] = &["INBOX"];

const SAMPLES: &[Sample] = &[
    Sample {
        from: MAYA,
        kind: Kind::Person,
        subject: "Design review moved to Thursday",
        text: "Hey! Quick heads up — the room was double-booked so I moved us to Thursday 2pm. I also added the new onboarding flows to the deck.",
        hours_ago: 1,
        unread: true,
        starred: false,
        earlier: &[false, true],
        attachment: Some(("onboarding-v3.pdf", "application/pdf", 96_211)),
        folders: &["INBOX", "[Gmail]/Important"],
    },
    Sample {
        from: ("GitHub", "notifications@github.com"),
        kind: Kind::GitHub,
        subject: "[envoy] Pull request #42: Faster thread loading",
        text: "jordan-lee requested your review on this pull request. Loads conversations from the local cache before the network round trip.",
        hours_ago: 2,
        unread: true,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: ("Lufthansa", "booking@lufthansa.com"),
        kind: Kind::Receipt,
        subject: "Your booking confirmation: SFO → MUC",
        text: "Thank you for booking with us. Your e-ticket and itinerary are attached. Check-in opens 23 hours before departure.",
        hours_ago: 5,
        unread: false,
        starred: true,
        earlier: &[],
        attachment: Some(("e-ticket-SFO-MUC.pdf", "application/pdf", 58_880)),
        folders: &["INBOX", "Travel"],
    },
    Sample {
        from: SAM,
        kind: Kind::Person,
        subject: "Dinner Saturday?",
        text: "We're doing tacos at our place around 7. Bring the kids if you want — Leo keeps asking when he can show off his new bike.",
        hours_ago: 9,
        unread: true,
        starred: false,
        earlier: &[false, true],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: ("Linear", "updates@linear.app"),
        kind: Kind::Newsletter("linear.app"),
        subject: "Weekly digest: 12 issues closed",
        text: "Here's what your team shipped this week. ENG-311 Offline queue retries, ENG-298 Search operators, ENG-305 Compact density.",
        hours_ago: 26,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: PRIYA,
        kind: Kind::Person,
        subject: "Re: Q3 roadmap draft",
        text: "Thanks for pulling this together. I left comments on the sync section — I think we can drop the second milestone if push works.",
        hours_ago: 30,
        unread: false,
        starred: true,
        earlier: &[true, false, true, false],
        attachment: None,
        folders: &["INBOX", "Work"],
    },
    Sample {
        from: ("Stripe", "receipts@stripe.com"),
        kind: Kind::Receipt,
        subject: "Your receipt from Fly.io #2291-4410",
        text: "Amount paid $23.40. Date paid Mar 2, 2026. Payment method Visa ending 4242.",
        hours_ago: 50,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: Some(("receipt-2291-4410.pdf", "application/pdf", 38_912)),
        folders: &["INBOX", "Receipts"],
    },
    Sample {
        from: ALEX,
        kind: Kind::Person,
        subject: "photos from the trip!!",
        text: "Finally uploaded everything. The one of you on the ridge at sunrise is ridiculous. Album link inside.",
        hours_ago: 76,
        unread: false,
        starred: false,
        earlier: &[false],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: ("The Browser Company", "hello@thebrowser.company"),
        kind: Kind::Newsletter("thebrowser.company"),
        subject: "What we've been building",
        text: "A short note from the team on what's next, and a few things we learned along the way.",
        hours_ago: 120,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: ("Dr. Rivera's Office", "frontdesk@riveradental.com"),
        kind: Kind::Automated,
        subject: "Appointment reminder",
        text: "This is a reminder of your appointment on Monday, March 9 at 10:30 AM. Reply C to confirm.",
        hours_ago: 150,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: ("Hacker Newsletter", "kale@hackernewsletter.com"),
        kind: Kind::Newsletter("hackernewsletter.com"),
        subject: "Hacker Newsletter #712",
        text: "The best articles from Hacker News this week: native UI is back, SQLite in production, and a love letter to IMAP.",
        hours_ago: 200,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: INBOX,
    },
    Sample {
        from: JORDAN,
        kind: Kind::Group,
        subject: "Offsite agenda",
        text: "Draft agenda for the offsite. Day one is planning, day two is hack day. Let me know what's missing.",
        hours_ago: 260,
        unread: false,
        starred: false,
        earlier: &[false],
        attachment: Some(("offsite-agenda.pdf", "application/pdf", 46_256)),
        folders: INBOX,
    },
    // Archived.
    Sample {
        from: ("Fly.io", "billing@fly.io"),
        kind: Kind::Receipt,
        subject: "Your invoice for February",
        text: "Your invoice for February is ready. Total due: $21.10, charged to Visa ending 4242.",
        hours_ago: 24 * 9,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: &["Receipts"],
    },
    Sample {
        from: ("Free Cruise Center", "winner@cruise-prizes.example"),
        kind: Kind::Person,
        subject: "You've been selected for a FREE cruise!!!",
        text: "Congratulations! Claim your all-inclusive Caribbean cruise today. Offer ends at midnight.",
        hours_ago: 40,
        unread: true,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: &["[Gmail]/Spam"],
    },
    // Snoozed: Snoozed's buckets, for when they wake ([`SNOOZED`]).
    Sample {
        from: ("City Library", "notices@library.example"),
        kind: Kind::Automated,
        subject: "Your hold is ready for pickup",
        text: "The book you reserved is waiting at the front desk until Friday.",
        hours_ago: 20,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: None,
        folders: &["snoozed:tomorrow"],
    },
    Sample {
        from: NORA,
        kind: Kind::Person,
        subject: "Invoice #118 — due next month",
        text: "Attached is the invoice for March. No rush, it's due on the 30th.",
        hours_ago: 48,
        unread: false,
        starred: false,
        earlier: &[],
        attachment: Some(("invoice-118.pdf", "application/pdf", 64_512)),
        folders: &["snoozed:nextweek"],
    },
];

/// The newest message of the first conversation, in Markdown-ish HTML, for the reader.
const DESIGN_HTML: &str = "<p>Hey!</p><p>Quick heads up — the room was double-booked so I moved us to <b>Thursday at 2pm</b>. Same Zoom link if you're remote.</p><p>I also added the new onboarding flows to the deck:</p><ul><li>Welcome screen with account picker</li><li>Gmail app-password walkthrough</li><li>First sync progress (with the <i>download ahead</i> setting)</li></ul><p>The prototype is here: <a href=\"https://figma.com/file/envoy-onboarding\">figma.com/file/envoy-onboarding</a></p><p>Thanks,<br>Maya</p>";

/// A draft waiting in Drafts.
const DRAFT: (&str, &str, &str) = (
    "Offsite travel plan",
    "Hi Jordan,\n\nThinking we take the 8:10 train on Thursday and share a car from the station. Could you check whether the hotel does early check-in?\n\nThanks!",
    "train-times.pdf",
);

/// The sample draft as the composer has it, without its file.
pub fn draft() -> envoy_core::model::Draft {
    envoy_core::model::Draft { account: ACCOUNT.into(), to: format!("{} <{}>", JORDAN.0, JORDAN.1), subject: DRAFT.0.into(), body: DRAFT.1.into(), ..Default::default() }
}

/// A snoozed sample's folder: the bucket for its preset's time, from now.
fn folder_of(name: &str, now: &chrono::DateTime<chrono::Local>) -> String {
    match name.strip_prefix("snoozed:") {
        Some(preset) => {
            let at = envoy_core::snooze::presets(now).into_iter().find(|p| p.id == preset).map_or(now.timestamp() + 86_400, |p| p.at);
            envoy_core::snooze::bucket(envoy_core::snooze::PARENT, "/", at)
        }
        None => name.to_owned(),
    }
}

fn person(p: (&str, &str)) -> (String, String) {
    (p.0.to_owned(), p.1.to_owned())
}

fn first_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// The demo's server, with the sample mail on it, dated from `now`.
pub fn server(now: chrono::DateTime<chrono::Local>) -> Server {
    let server = Server::new();
    let me = account();
    server.add_account(me, FOLDERS);
    let html = std::env::var("ENVOY_DEMO_HTML").ok().and_then(|p| std::fs::read_to_string(p).ok());
    for (i, s) in SAMPLES.iter().enumerate() {
        let newest = now.timestamp() - s.hours_ago * 3600 - 600;
        let folders: Vec<String> = s.folders.iter().map(|f| folder_of(f, &now)).collect();
        let n = s.earlier.len() as i64;
        let mut refs: Vec<String> = Vec::new();
        let base = s.subject.strip_prefix("Re: ").unwrap_or(s.subject);
        // The conversation, oldest first: the newest is theirs, and yours are in Sent.
        for (j, &mine) in s.earlier.iter().chain([&false]).enumerate() {
            let last = j as i64 == n;
            let date = newest - (n - j as i64) * 5400;
            let (from, to) = if mine { (person(ME), vec![person(s.from)]) } else { (person(s.from), vec![person(ME)]) };
            // To a group you're copied on; Maya copies Jordan in (your replies are to her alone).
            let (to, cc) = match s.kind {
                Kind::Group => (vec![("Northwind team".into(), "team@northwind.dev".into())], vec![person(ME)]),
                _ if !mine && s.from == MAYA => (to, vec![person(JORDAN)]),
                _ => (to, Vec::new()),
            };
            let subject = match j {
                _ if last => s.subject.to_owned(),
                0 => base.to_owned(),
                _ => format!("Re: {base}"),
            };
            // People sign with their first name; companies and machines with all of theirs.
            let signed = if matches!(s.kind, Kind::Person | Kind::Group) { first_name(s.from.0) } else { s.from.0 };
            let text = if last {
                format!("{}\n\nSee you soon,\n{signed}", s.text)
            } else if mine {
                "Sounds good — I'll take a look this afternoon.\n\nYou".to_owned()
            } else {
                format!("Sharing an early version so we can talk it through before the review.\n\nBest,\n{}", first_name(s.from.0))
            };
            let mut mail = Mail {
                from,
                to,
                cc,
                subject,
                date,
                message_id: format!("s{i}.{j}@demo.envoy"),
                references: refs.clone(),
                headers: headers(s.kind),
                text: Some(text),
                html: None,
                attachment: if last { s.attachment.map(|(n, m, z)| (n.into(), m.into(), z)) } else { None },
            };
            if last && i == 0 {
                mail.html = Some(html.clone().unwrap_or_else(|| DESIGN_HTML.to_owned()));
                if html.is_some() {
                    mail.text = None;
                }
            }
            refs.push(mail.message_id.clone());
            let raw = mail.rfc822();
            let mut fl = if last && s.unread { 0 } else { flags::SEEN };
            if last && s.starred {
                fl |= flags::FLAGGED;
            }
            if mine {
                server.add_mail(ACCOUNT, &["[Gmail]/Sent Mail"], raw, flags::SEEN, date);
            } else {
                let folders: Vec<&str> = folders.iter().map(String::as_str).collect();
                server.add_mail(ACCOUNT, &folders, raw, fl, date);
            }
        }
    }
    let (subject, body, file) = DRAFT;
    let draft = Mail {
        from: person(ME),
        to: vec![person(JORDAN)],
        subject: subject.into(),
        date: now.timestamp() - 3 * 3600,
        message_id: "draft.1@demo.envoy".into(),
        text: Some(body.into()),
        attachment: Some((file.into(), "application/pdf".into(), 24_400)),
        ..Mail::default()
    };
    server.add_mail(ACCOUNT, &[DRAFTS], draft.rfc822(), flags::SEEN | flags::DRAFT, draft.date);
    server
}

fn headers(kind: Kind) -> Vec<(String, String)> {
    let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect();
    match kind {
        Kind::Person | Kind::Group | Kind::Receipt => Vec::new(),
        Kind::Newsletter(list) => h(&[("List-Id", &format!("<news.{list}>")), ("List-Unsubscribe", &format!("<https://{list}/unsubscribe>"))]),
        Kind::GitHub => h(&[("List-Id", "envoy <envoy.example.github.com>"), ("X-GitHub-Reason", "review_requested"), ("List-Unsubscribe", "<https://github.com/notifications/unsubscribe>")]),
        Kind::Automated => h(&[("Auto-Submitted", "auto-generated")]),
    }
}

/// Puts a sample message in a snooze bucket whose time came `ago` seconds before `now`: the
/// engine wakes it on its next check, back atop the inbox. For the `woken` scene.
pub fn snoozed_till(server: &Server, now: i64, ago: i64) {
    let at = now - ago;
    let mail = Mail {
        from: person(("Kiko Tanaka", "kiko@tanaka.example")),
        to: vec![person(ME)],
        subject: "Can you send the slides before Friday?".into(),
        date: at - 86_400,
        message_id: "woken.1@demo.envoy".into(),
        text: Some("No rush, but I'd love to look them over before the call.\n\nKiko".into()),
        ..Mail::default()
    };
    server.add_mail(ACCOUNT, &[&envoy_core::snooze::bucket(envoy_core::snooze::PARENT, "/", at)], mail.rfc822(), flags::SEEN, mail.date);
}

/// Starts the app's engine on the demo's server (filled with the samples, dated from now), its
/// cache in a folder of its own: the real engine, all of it, offline.
pub fn start(app: &mut App, wake: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
    let server = Arc::new(server(chrono::Local::now()));
    let engine = Engine::start_offline(wake, app.settings.clone(), server.clone(), database()).map_err(|e| format!("{e:#}"))?;
    app.engine = Some(engine);
    app.demo_server = Some(server);
    Ok(())
}

/// A cache of the demo's own, never used before: each demo starts from the samples.
fn database() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = envoy_core::paths::data_dir().join(format!("demo-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir.join("mail.sqlite")
}

#[cfg(test)]
pub mod testing {
    use envoy_core::engine::Command;

    use crate::app::App;

    /// An app on demo mode's engine, its inbox listed: the real engine and store, on the sample
    /// mail, as `--demo` opens.
    pub fn app() -> App {
        with(|_| {})
    }

    /// [`app`], `change`d before its engine starts (settings, say).
    pub fn with(change: impl FnOnce(&mut App)) -> App {
        let mut app = App::new(true);
        change(&mut app);
        super::start(&mut app, || {}).expect("the demo's engine starts");
        let view = app.view.clone();
        app.request_list();
        app.send(Command::Watch { view });
        assert!(app.settle(), "the demo's engine answers");
        app
    }

    /// [`app`], its engine let go: what's asked of it is only recorded (`App::sent`), and nothing
    /// answers, so a test can answer as the engine would.
    pub fn detached() -> App {
        let mut app = app();
        app.engine = None;
        app
    }
}
