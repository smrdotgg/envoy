//! Sample mail for `--demo` and for headless screenshots: the whole UI without an account.

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::model::{Address, Attachment, Body, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};

use crate::app::App;

const ACCOUNT: &str = "demo";

fn addr(name: &str, email: &str) -> Address {
    Address { name: name.into(), email: email.into() }
}

struct Sample {
    from: (&'static str, &'static str),
    subject: &'static str,
    preview: &'static str,
    hours_ago: i64,
    unread: bool,
    starred: bool,
    count: u32,
    attach: bool,
    mailbox: &'static str,
}

const SAMPLES: &[Sample] = &[
    Sample { from: ("Maya Chen", "maya@northwind.dev"), subject: "Design review moved to Thursday", preview: "Hey! Quick heads up — the room was double-booked so I moved us to Thursday 2pm. I also added the new onboarding flows to the deck.", hours_ago: 1, unread: true, starred: false, count: 3, attach: true, mailbox: "INBOX" },
    Sample { from: ("GitHub", "notifications@github.com"), subject: "[envoy] Pull request #42: Faster thread loading", preview: "jordan-lee requested your review on this pull request. Loads conversations from the local cache before the network round trip.", hours_ago: 2, unread: true, starred: false, count: 1, attach: false, mailbox: "INBOX" },
    Sample { from: ("Lufthansa", "booking@lufthansa.com"), subject: "Your booking confirmation: SFO → MUC", preview: "Thank you for booking with us. Your e-ticket and itinerary are attached. Check-in opens 23 hours before departure.", hours_ago: 5, unread: false, starred: true, count: 1, attach: true, mailbox: "INBOX" },
    Sample { from: ("Sam Ortiz", "sam@ortiz.family"), subject: "Dinner Saturday?", preview: "We're doing tacos at our place around 7. Bring the kids if you want — Leo keeps asking when he can show off his new bike.", hours_ago: 9, unread: true, starred: false, count: 4, attach: false, mailbox: "INBOX" },
    Sample { from: ("Linear", "updates@linear.app"), subject: "Weekly digest: 12 issues closed", preview: "Here's what your team shipped this week. ENG-311 Offline queue retries, ENG-298 Search operators, ENG-305 Compact density.", hours_ago: 26, unread: false, starred: false, count: 1, attach: false, mailbox: "INBOX" },
    Sample { from: ("Priya Patel", "priya@acme.co"), subject: "Re: Q3 roadmap draft", preview: "Thanks for pulling this together. I left comments on the sync section — I think we can drop the second milestone if push works.", hours_ago: 30, unread: false, starred: true, count: 6, attach: false, mailbox: "INBOX" },
    Sample { from: ("Stripe", "receipts@stripe.com"), subject: "Your receipt from Fly.io #2291-4410", preview: "Amount paid $23.40. Date paid Mar 2, 2026. Payment method Visa ending 4242.", hours_ago: 50, unread: false, starred: false, count: 1, attach: true, mailbox: "INBOX" },
    Sample { from: ("Alex Kim", "alex.kim@gmail.com"), subject: "photos from the trip!!", preview: "Finally uploaded everything. The one of you on the ridge at sunrise is ridiculous. Album link inside.", hours_ago: 76, unread: false, starred: false, count: 2, attach: false, mailbox: "INBOX" },
    Sample { from: ("The Browser Company", "hello@thebrowser.company"), subject: "What we've been building", preview: "A short note from the team on what's next, and a few things we learned along the way.", hours_ago: 120, unread: false, starred: false, count: 1, attach: false, mailbox: "INBOX" },
    Sample { from: ("Dr. Rivera's Office", "frontdesk@riveradental.com"), subject: "Appointment reminder", preview: "This is a reminder of your appointment on Monday, March 9 at 10:30 AM. Reply C to confirm.", hours_ago: 150, unread: false, starred: false, count: 1, attach: false, mailbox: "INBOX" },
    Sample { from: ("Hacker Newsletter", "kale@hackernewsletter.com"), subject: "Hacker Newsletter #712", preview: "The best articles from Hacker News this week: native UI is back, SQLite in production, and a love letter to IMAP.", hours_ago: 200, unread: false, starred: false, count: 1, attach: false, mailbox: "INBOX" },
    Sample { from: ("Jordan Lee", "jordan@northwind.dev"), subject: "Offsite agenda", preview: "Draft agenda for the offsite. Day one is planning, day two is hack day. Let me know what's missing.", hours_ago: 260, unread: false, starred: false, count: 2, attach: true, mailbox: "INBOX" },
    Sample { from: ("You", "you@gmail.com"), subject: "Re: Design review moved to Thursday", preview: "Thursday works. I'll bring the prototype on my laptop.", hours_ago: 0, unread: false, starred: false, count: 1, attach: false, mailbox: "[Gmail]/Sent Mail" },
];

pub fn account() -> Account {
    Account {
        id: ACCOUNT.into(),
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

fn mailbox(name: &str, display: &str, role: Role, unread: u32) -> Mailbox {
    Mailbox { account: ACCOUNT.into(), name: name.into(), display: display.into(), role, unread, total: 100, depth: 0, selectable: true }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn summary(i: usize, s: &Sample) -> Summary {
    let mut fl = 0;
    if !s.unread {
        fl |= flags::SEEN;
    }
    if s.starred {
        fl |= flags::FLAGGED;
    }
    let participants = match s.count {
        1 => String::new(),
        2 => format!("{}, me", first(s.from.0)),
        _ => format!("{}, me, Jordan", first(s.from.0)),
    };
    Summary {
        id: i as MessageId + 1,
        account: ACCOUNT.into(),
        mailbox: s.mailbox.into(),
        uid: i as u32 + 1,
        thread: format!("t{i}"),
        subject: s.subject.into(),
        from: addr(s.from.0, s.from.1),
        to: vec![addr("You", "you@gmail.com")],
        date: now() - s.hours_ago * 3600 - 600,
        flags: fl,
        preview: s.preview.into(),
        has_attachments: s.attach,
        thread_count: s.count,
        participants,
        thread_unread: s.unread,
    }
}

fn first(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

pub fn list_for(view: &View) -> Vec<Summary> {
    SAMPLES
        .iter()
        .enumerate()
        .map(|(i, s)| summary(i, s))
        .filter(|m| match view {
            View::Role { role: Role::Inbox, .. } => m.mailbox == "INBOX",
            View::Role { role: Role::Sent, .. } => m.mailbox.contains("Sent"),
            View::Role { role: Role::Flagged, .. } | View::Starred { .. } => m.flagged(),
            View::Mailbox { mailbox, .. } => &m.mailbox == mailbox,
            _ => false,
        })
        .collect()
}

const DESIGN_MD: &str = "Hey!\n\nQuick heads up — the room was double-booked so I moved us to **Thursday at 2pm**. Same Zoom link if you're remote.\n\nI also added the new onboarding flows to the deck:\n\n- Welcome screen with account picker\n- Gmail app-password walkthrough\n- First sync progress (with the *download ahead* setting)\n\nThe prototype is here: [figma.com/file/envoy-onboarding](https://figma.com/file/envoy-onboarding)\n\nThanks,\\\nMaya";

pub fn thread_for(id: MessageId) -> Vec<ThreadMessage> {
    let i = (id - 1) as usize;
    let Some(s) = SAMPLES.get(i) else { return vec![] };
    let base = summary(i, s);
    let mut out = Vec::new();
    let n = s.count.max(1) as i64;
    for j in 0..n {
        let last = j == n - 1;
        let mut m = base.clone();
        m.id = id * 100 + j;
        m.date = base.date - (n - 1 - j) * 5400;
        m.thread_count = 1;
        let mine = j % 2 == 1;
        if mine {
            m.from = addr("You", "you@gmail.com");
            m.to = vec![base.from.clone()];
            m.preview = "Sounds good — I'll take a look this afternoon.".into();
        } else if !last {
            m.preview = "Sharing an early version so we can talk it through before the review.".into();
        }
        if !last {
            m.flags |= flags::SEEN;
        }
        let html_path = std::env::var("ENVOY_DEMO_HTML").ok().filter(|_| last && i == 0);
        let (text, markdown) = if html_path.is_some() {
            (None, None)
        } else if last && i == 0 {
            (None, Some(DESIGN_MD.to_owned()))
        } else if last {
            (Some(format!("{}\n\nSee you soon,\n{}\n\nOn Tue, You wrote:\n> Earlier message, quoted.", s.preview, first(s.from.0))), None)
        } else {
            (Some(format!("{}\n\nBest,\n{}", m.preview, if mine { "You" } else { first(s.from.0) })), None)
        };
        let attachments = if last && s.attach {
            vec![Attachment { index: 0, filename: if i == 2 { "e-ticket-SFO-MUC.pdf".into() } else { "onboarding-v3.fig".into() }, mime: "application/pdf".into(), size: 482_113 }]
        } else {
            vec![]
        };
        out.push(ThreadMessage {
            body: Some(Body {
                id: m.id,
                message_id: format!("<{}@demo>", m.id),
                references: String::new(),
                cc: if last && s.count > 2 { vec![addr("Jordan Lee", "jordan@northwind.dev")] } else { vec![] },
                reply_to: vec![],
                text,
                doc: html_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|h| std::sync::Arc::new(envoy_core::html::parse(&h))),
                html: None,
                markdown,
                attachments,
            }),
            summary: m,
        });
    }
    out
}

pub fn populate(app: &mut App) {
    app.accounts = vec![account()];
    app.mailboxes.insert(
        ACCOUNT.into(),
        vec![
            mailbox("INBOX", "Inbox", Role::Inbox, 3),
            mailbox("[Gmail]/Starred", "Starred", Role::Flagged, 0),
            mailbox("[Gmail]/Important", "Important", Role::Important, 1),
            mailbox("[Gmail]/Sent Mail", "Sent", Role::Sent, 0),
            mailbox("[Gmail]/Drafts", "Drafts", Role::Drafts, 0),
            mailbox("[Gmail]/All Mail", "All Mail", Role::All, 0),
            mailbox("[Gmail]/Spam", "Spam", Role::Junk, 0),
            mailbox("[Gmail]/Trash", "Trash", Role::Trash, 0),
            Mailbox { depth: 0, ..mailbox("Receipts", "Receipts", Role::Other, 0) },
            Mailbox { depth: 0, ..mailbox("Travel", "Travel", Role::Other, 1) },
            Mailbox { depth: 0, ..mailbox("Work", "Work", Role::Other, 0) },
        ],
    );
    app.sync.insert(ACCOUNT.into(), SyncState::Idle);
    app.view = View::Role { account: Some(ACCOUNT.into()), role: Role::Inbox };
    app.list = list_for(&app.view);
    app.list_loaded = true;
}
