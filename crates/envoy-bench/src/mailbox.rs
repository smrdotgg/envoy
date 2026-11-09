//! A large, deterministic synthetic mailbox, written through the same `Store` calls the IMAP
//! sync uses (`upsert_header` + `set_body` on parsed raw RFC 822), so the schema, FTS index and
//! previews are exactly what a real cache holds.
//!
//! Shape (100k messages by default): one account over ~6 years; Inbox, Archive (the bulk), Sent,
//! Drafts, Trash, Spam and two user folders; ~3000 people with a heavy-tailed sender
//! distribution plus bots and newsletters; a third of messages are replies in conversations of
//! 2–12; ~15% HTML newsletters and receipts (some HTML-only); ~9% with attachments; unread mail
//! concentrated in the recent inbox. Every message has its body cached (the worst case for
//! search: the full-text index covers all of them). The same seed always yields the same bytes.
//!
//! For the split inbox, mail carries the headers real mail does: newsletters `List-Unsubscribe`
//! and `List-Id`, bots `Auto-Submitted`; some people's mail goes to a team address with you
//! copied. The store sorts it as it's written, with the classifier the engine would set for this
//! account (no rules, its address), so a run starts with nothing left to sort.

use std::path::Path;

use anyhow::{Context, Result};
use envoy_core::mime;
use envoy_core::model::{Role, flags};
use envoy_core::store::Store;

pub const ACCOUNT: &str = "bench";
pub const ME: &str = "me@envoy-bench.test";
/// The newest message's date: fixed, so runs are comparable.
pub const END: i64 = 1_793_491_200; // 2026-11-01T00:00:00Z
const SPAN: i64 = 6 * 365 * 86_400;

/// SplitMix64: tiny, fast, and identical on every platform.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.f() * n as f64) as usize % n.max(1)
    }
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.f() < p
    }
    /// Heavy-tailed pick: low indices are much more common (a few people send most mail).
    pub fn skewed(&mut self, n: usize) -> usize {
        let u = self.f();
        ((u * u * u) * n as f64) as usize % n.max(1)
    }
    /// Zipf (exponent 1) over `n` ranks: rank r with probability ≈ 1 / ((r + 1.5) ln n).
    pub fn zipf(&mut self, n: usize) -> usize {
        (((n + 1) as f64).powf(self.f()) as usize).saturating_sub(1).min(n - 1)
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

pub const FIRST: &[&str] = &[
    "Alice", "Sam", "Maya", "Jonas", "Priya", "Omar", "Lena", "Diego", "Hannah", "Kenji", "Fatima", "Lucas", "Zoë", "Mateo", "Aisha",
    "Noah", "Sofia", "Elias", "Chloé", "Ravi", "Ingrid", "Tomás", "Mei", "Oliver", "Nadia", "Felix", "Amara", "Leo", "Yuki", "Grace",
    "Hugo", "Isabel", "Arjun", "Clara", "Mohammed", "Freya", "José", "Elena", "Marcus", "Søren", "Leila", "Daniel", "Anya", "Theo",
    "Rosa", "Kofi", "Emma", "Ivan", "Nora", "Ahmed", "Julia", "Pablo", "Sara", "Victor", "Hana", "Ben", "Lucía", "Kwame", "Ines", "Max",
];
pub const LAST: &[&str] = &[
    "Chen", "Okafor", "Lindqvist", "Patel", "García", "Novak", "Schmidt", "Kim", "Rossi", "Müller", "Haddad", "Silva", "Tanaka",
    "Johansson", "Nguyen", "Dubois", "Kowalski", "Andersen", "Moreau", "Ivanova", "Costa", "Fischer", "Yilmaz", "Brown", "Wójcik",
    "Okonkwo", "Hernández", "Larsen", "Sato", "Ferreira", "Bauer", "Murphy", "Romano", "Kaur", "Svensson", "O'Brien", "Petrov", "Ali",
    "Martin", "Lopez", "Wright", "Becker", "Jensen", "Nakamura", "Abara", "Vargas", "Weber", "Singh", "Popescu", "Hughes",
];
const COMPANY: &[&str] = &[
    "northwind", "contoso", "fabrikam", "tailspin", "wingtip", "adventureworks", "litware", "proseware", "lucerne", "margie",
    "fourthcoffee", "woodgrove", "alpineski", "blueyonder", "coho", "humongous", "treyresearch", "wideworld", "graphicdesign", "datum",
];
const PERSONAL: &[&str] = &["mail.test", "inbox.test", "post.test", "fastmail.test", "proton.test"];
const BOTS: &[(&str, &str)] = &[
    ("Forge", "notifications@forge.example"),
    ("Build bot", "ci@builds.example"),
    ("Calendar", "calendar-noreply@cal.example"),
    ("Tracker", "issues@tracker.example"),
    ("Security alerts", "security@accounts.example"),
    ("Docs", "comments@docs.example"),
    ("Shipping updates", "track@parcels.example"),
    ("Bank of Lucerne", "alerts@lucerne-bank.example"),
];
const NEWSLETTERS: &[(&str, &str)] = &[
    ("Northwind Weekly", "news@northwind.example"),
    ("The Rust Digest", "digest@rustnews.example"),
    ("Design Notes", "hello@designnotes.example"),
    ("Contoso Deals", "deals@contoso.example"),
    ("Morning Brief", "brief@morning.example"),
    ("Product Hunt-ish", "daily@launches.example"),
    ("Travel Picks", "picks@wanderlust.example"),
    ("City Events", "events@citylife.example"),
    ("Running Club", "club@runners.example"),
    ("Cooking at Home", "recipes@homecook.example"),
];
const SHOPS: &[(&str, &str)] = &[
    ("Acme Store", "orders@acme.example"),
    ("Fabrikam Billing", "billing@fabrikam.example"),
    ("Tailspin Air", "receipts@tailspin.example"),
    ("Coho Cloud", "invoices@cohocloud.example"),
    ("Lucerne Energy", "billing@lucerne-energy.example"),
];
const PROJECTS: &[&str] = &[
    "Atlas", "Borealis", "Cobalt", "Driftwood", "Ember", "Falcon", "Granite", "Harbor", "Iris", "Juniper", "Kestrel", "Lumen", "Meridian",
    "Nimbus", "Orchid", "Pioneer", "Quartz", "Redwood", "Sierra", "Tundra",
];
const TOPICS: &[&str] = &[
    "budget review", "hiring plan", "launch checklist", "roadmap", "offsite agenda", "design review", "contract renewal", "quarterly report",
    "customer feedback", "pricing update", "security audit", "onboarding docs", "performance review", "migration plan", "incident report",
    "sprint planning", "vendor proposal", "press release", "travel booking", "team dinner", "board deck", "API changes", "release notes",
    "user research", "support escalation", "lease agreement", "tax documents", "school pickup", "birthday party", "apartment viewing",
];
const DAYS: &[&str] = &["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
/// The most frequent words (≈55% of body text), picked heavy-tailed.
const FUNCTION: &[&str] = &[
    "the", "be", "to", "of", "and", "a", "in", "that", "have", "it", "for", "not", "on", "with", "as", "you", "do", "at", "this", "but",
    "by", "from", "they", "we", "say", "her", "she", "or", "an", "will", "my", "one", "all", "would", "there", "their", "what", "so",
    "up", "out", "if", "about", "who", "get", "which", "go", "me", "when", "make", "can", "like", "time", "no", "just", "him", "know",
    "take", "people", "into", "year", "your", "good", "some", "could", "them", "see", "other", "than", "then", "now", "look", "only",
    "come", "its", "over", "think", "also", "back", "after", "use", "two", "how", "our", "work", "first", "well", "way", "even", "new",
    "want", "because", "any", "these", "give", "day", "most", "us",
];

/// Content words, most common first. Each sits at a fixed rank of a Zipf vocabulary whose long
/// tail is made-up words (names, jargon, codes in real mail), so document frequencies come out
/// realistic: "meeting" in roughly a quarter of messages, "invoice" in a few percent, "pancakes"
/// in a handful.
const CONTENT: &[&str] = &[
    "thanks", "please", "meeting", "team", "week", "call", "update", "project", "review", "next", "tomorrow", "notes", "question",
    "draft", "schedule", "plan", "feedback", "client", "customer", "office", "morning", "afternoon", "report", "agenda", "budget",
    "deadline", "design", "proposal", "release", "issue", "fix", "support", "ticket", "order", "payment", "account", "travel", "lunch",
    "dinner", "coffee", "weekend", "family", "school", "flight", "hotel", "invoice", "contract", "launch", "estimate", "timeline",
    "approval", "decision", "summary", "priority", "blocker", "milestone", "scope", "risk", "cost", "revenue", "forecast", "hiring",
    "candidate", "interview", "offer", "quarter", "numbers", "server", "database", "documents", "signature", "delivery", "package",
    "receipt", "refund", "subscription", "renewal", "discount", "security", "password", "insurance", "lease", "apartment", "doctor",
    "appointment", "holiday", "photos", "concert", "museum", "hiking", "garden", "piano", "bicycle", "marathon", "recipe", "volunteer",
    "library", "quickly", "really", "probably", "definitely", "honestly", "already", "still", "almost", "maybe", "exactly",
    "important", "different", "possible", "available", "interesting", "difficult", "simple", "great", "small", "large", "early",
    "late", "final", "attached", "follow", "pancakes",
];

const VOCAB_SIZE: usize = 40_000;

/// The content vocabulary, by Zipf rank: `VOCAB_SIZE` distinct tokens. Made-up words fill the
/// ranks, shortest (most frequent) first, each from a bijective syllable code within its length
/// and checked against every other token; the real content words then take their fixed ranks.
fn vocab() -> &'static [String] {
    static V: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        const SYL: &[&str] = &["ka", "lo", "mi", "ren", "to", "sa", "vel", "dor", "ni", "pa", "qu", "ri", "zan", "bo", "ti", "lu", "mer", "sha", "go", "fe"];
        let mut used: std::collections::HashSet<String> = FUNCTION.iter().chain(CONTENT).map(|w| w.to_string()).collect();
        let mut v: Vec<String> = Vec::with_capacity(VOCAB_SIZE);
        'fill: for len in 2..=5u32 {
            let class = 20u64.pow(len);
            for i in 0..class {
                if v.len() == VOCAB_SIZE {
                    break 'fill;
                }
                // 7919 is prime and shares no factor with 20^len: a permutation of the class.
                let mut n = (i * 7919 + 13) % class;
                let w: String = (0..len)
                    .map(|_| {
                        let s = SYL[(n % 20) as usize];
                        n /= 20;
                        s
                    })
                    .collect();
                if used.insert(w.clone()) {
                    v.push(w);
                }
            }
        }
        for (i, w) in CONTENT.iter().enumerate() {
            v[content_rank(i)] = w.to_string();
        }
        v
    })
}

/// Where the `i`th content word sits in the vocabulary.
fn content_rank(i: usize) -> usize {
    10 + i + (10.0 * 1.06f64.powi(i as i32)) as usize
}

#[derive(Clone)]
struct Person {
    name: String,
    email: String,
}

fn people(rng: &mut Rng, n: usize) -> Vec<Person> {
    let mut out = Vec::with_capacity(n);
    let mut seen = std::collections::HashSet::new();
    while out.len() < n {
        let f = rng.pick(FIRST);
        let l = rng.pick(LAST);
        let domain = if rng.chance(0.6) { format!("{}.example", rng.pick(COMPANY)) } else { rng.pick(PERSONAL).to_string() };
        let local: String = format!("{}.{}", f, l).to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
        let email = format!("{local}@{domain}");
        if seen.insert(email.clone()) {
            out.push(Person { name: format!("{f} {l}"), email });
        }
    }
    out
}

fn sentence(rng: &mut Rng) -> String {
    let n = rng.range(6, 18);
    let mut s = String::new();
    for i in 0..n {
        let w: &str = if rng.chance(0.55) { FUNCTION[rng.skewed(FUNCTION.len())] } else { &vocab()[rng.zipf(VOCAB_SIZE)] };
        if i == 0 {
            let mut c = w.chars();
            if let Some(f) = c.next() {
                s.extend(f.to_uppercase());
                s.push_str(c.as_str());
            }
        } else {
            s.push(' ');
            s.push_str(w);
        }
    }
    // A rare token now and then (order numbers, codes): keeps the index realistically wide.
    if rng.chance(0.15) {
        s.push_str(&format!(" ref {}-{}", rng.pick(PROJECTS).to_uppercase(), rng.range(1000, 99_999)));
    }
    s.push(if rng.chance(0.15) { '?' } else { '.' });
    s
}

fn paragraphs(rng: &mut Rng, lo: usize, hi: usize) -> Vec<String> {
    (0..rng.range(lo, hi)).map(|_| (0..rng.range(1, 5)).map(|_| sentence(rng)).collect::<Vec<_>>().join(" ")).collect()
}

fn rfc2822(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.to_rfc2822()).unwrap_or_default()
}

fn addr(name: &str, email: &str) -> String {
    format!("{name} <{email}>")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A table-layout newsletter with inline styles, headings, lists and links, like real ones.
fn newsletter_html(rng: &mut Rng, title: &str, brand: &str) -> String {
    let mut h = String::with_capacity(24_000);
    h.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>body{margin:0;background:#f4f4f7}.btn{background:#5b5bd6;color:#fff;padding:10px 18px;border-radius:6px;text-decoration:none}td{font-family:Helvetica,Arial,sans-serif}</style></head><body>");
    h.push_str("<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f4f4f7\"><tr><td align=\"center\">");
    h.push_str("<table role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#ffffff;border-radius:8px\">");
    h.push_str(&format!("<tr><td style=\"padding:24px 32px;font-size:22px;font-weight:bold;color:#1c1c28\">{}</td></tr>", html_escape(brand)));
    h.push_str(&format!("<tr><td style=\"padding:0 32px 16px;font-size:28px;line-height:34px;color:#1c1c28\"><h1 style=\"margin:0;font-size:28px\">{}</h1></td></tr>", html_escape(title)));
    for _ in 0..rng.range(3, 9) {
        let heading = sentence(rng);
        h.push_str(&format!(
            "<tr><td style=\"padding:16px 32px 4px\"><h2 style=\"margin:0;font-size:18px;color:#333\">{}</h2></td></tr>",
            html_escape(heading.trim_end_matches(['.', '?']))
        ));
        for p in paragraphs(rng, 1, 3) {
            h.push_str(&format!("<tr><td style=\"padding:4px 32px;font-size:15px;line-height:22px;color:#444\"><p style=\"margin:0 0 12px\">{}</p></td></tr>", html_escape(&p)));
        }
        if rng.chance(0.5) {
            h.push_str("<tr><td style=\"padding:4px 32px\"><ul style=\"margin:0;padding-left:20px;color:#444;font-size:15px\">");
            for _ in 0..rng.range(2, 5) {
                h.push_str(&format!("<li style=\"margin:4px 0\">{}</li>", html_escape(&sentence(rng))));
            }
            h.push_str("</ul></td></tr>");
        }
        h.push_str(&format!(
            "<tr><td style=\"padding:8px 32px 16px\"><a class=\"btn\" href=\"https://{}.example/r/{}\" style=\"background:#5b5bd6;color:#ffffff;padding:10px 18px;border-radius:6px;text-decoration:none;display:inline-block\">Read more</a></td></tr>",
            brand.to_lowercase().replace(' ', ""),
            rng.next() % 1_000_000
        ));
    }
    h.push_str("<tr><td style=\"padding:24px 32px;font-size:12px;color:#888;border-top:1px solid #eee\">You are receiving this because you subscribed. <a href=\"https://example.com/unsubscribe\" style=\"color:#888\">Unsubscribe</a> · <a href=\"https://example.com/prefs\" style=\"color:#888\">Preferences</a><br>123 Example Street, Springfield</td></tr>");
    h.push_str("</table></td></tr></table></body></html>");
    h
}

fn receipt_html(rng: &mut Rng, shop: &str, order: u32) -> String {
    let mut rows = String::new();
    let mut total = 0u32;
    for _ in 0..rng.range(1, 8) {
        let cents = rng.range(299, 25_000) as u32;
        total += cents;
        rows.push_str(&format!(
            "<tr><td style=\"padding:6px 0\">{}</td><td align=\"right\" style=\"padding:6px 0\">${}.{:02}</td></tr>",
            html_escape(rng.pick(CONTENT)),
            cents / 100,
            cents % 100
        ));
    }
    format!(
        "<html><body style=\"font-family:Arial,sans-serif;color:#222\"><table width=\"560\" align=\"center\" cellpadding=\"0\" cellspacing=\"0\"><tr><td><h2>{}</h2><p>Thanks for your order <b>#{order}</b>. Your receipt is below.</p><table width=\"100%\" style=\"border-collapse:collapse;border-top:1px solid #ddd\">{rows}<tr><td style=\"padding:8px 0;border-top:1px solid #ddd\"><b>Total</b></td><td align=\"right\" style=\"padding:8px 0;border-top:1px solid #ddd\"><b>${}.{:02}</b></td></tr></table><p style=\"color:#666;font-size:12px\">Questions? Reply to this email or visit our help centre.</p></td></tr></table></body></html>",
        html_escape(shop),
        total / 100,
        total % 100
    )
}

const ATTACHMENTS: &[(&str, &str)] = &[
    ("report.pdf", "application/pdf"),
    ("invoice.pdf", "application/pdf"),
    ("photo.jpg", "image/jpeg"),
    ("slides.pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("budget.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("notes.txt", "text/plain"),
    ("contract.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
];

/// A small base64 payload: attachment metadata is what the list and reader use, not the bytes.
fn attachment_part(rng: &mut Rng, boundary: &str) -> String {
    let (name, mime) = rng.pick(ATTACHMENTS);
    let name = format!("{}-{}", rng.pick(PROJECTS).to_lowercase(), name);
    let bytes: Vec<u8> = (0..rng.range(96, 480)).map(|_| (rng.next() & 0xff) as u8).collect();
    use base64_lite::encode;
    format!("--{boundary}\r\nContent-Type: {mime}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", encode(&bytes))
}

mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut out = String::with_capacity(b.len() * 4 / 3 + 4);
        for (i, c) in b.chunks(3).enumerate() {
            if i > 0 && i % 19 == 0 {
                out.push_str("\r\n");
            }
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        out
    }
}

struct Thread {
    last_mid: String,
    refs: Vec<String>,
    subject: String,
    people: Vec<usize>,
    left: usize,
    last_text: String,
}

#[derive(Default)]
pub struct Summary {
    pub messages: usize,
    pub html: usize,
    pub with_attachments: usize,
    pub replies: usize,
    pub unread: usize,
    pub bytes_raw: usize,
}

const BOXES: &[(&str, &str, Role)] = &[
    ("INBOX", "Inbox", Role::Inbox),
    ("Archive", "Archive", Role::Archive),
    ("Sent", "Sent", Role::Sent),
    ("Drafts", "Drafts", Role::Drafts),
    ("Trash", "Trash", Role::Trash),
    ("Junk", "Spam", Role::Junk),
    ("Receipts", "Receipts", Role::Other),
    ("Work/Clients", "Clients", Role::Other),
];

/// Writes the mailbox, `accounts.json` and an empty `settings.json` into `home` (an
/// `ENVOY_HOME`). Refuses to overwrite an existing cache.
pub fn generate(home: &Path, count: usize, seed: u64) -> Result<Summary> {
    std::fs::create_dir_all(home)?;
    let db = home.join("mail.sqlite");
    anyhow::ensure!(!db.exists(), "{} already exists; delete it or pick another --home", db.display());
    let account = envoy_core::accounts::Account {
        id: ACCOUNT.into(),
        email: ME.into(),
        display_name: "Bench User".into(),
        auth: envoy_core::accounts::AuthKind::Password,
        username: ME.into(),
        // No secret is saved, so the sync worker stops at "no password" and never connects:
        // the run is fully offline, as a cold start without network would be.
        imap_host: "127.0.0.1".into(),
        imap_port: 9,
        smtp_host: "127.0.0.1".into(),
        smtp_port: 9,
        hue: 260.0,
        signature: String::new(),
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
    };
    std::fs::write(home.join("accounts.json"), serde_json::to_string_pretty(&serde_json::json!({ "accounts": [account] }))?)?;
    std::fs::write(home.join("settings.json"), "{}\n")?;

    let store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
    store.set_classifier(envoy_core::category::Classifier::new(Vec::new(), [ME.to_owned()]))?;
    for (name, display, role) in BOXES {
        store.upsert_mailbox(ACCOUNT, name, display, *role, "/", true)?;
    }
    let mut rng = Rng::new(seed);
    let folks = people(&mut rng, 3000);
    let me = Person { name: "Bench User".into(), email: ME.into() };
    let mut uids: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut counts: std::collections::HashMap<&str, (u32, u32)> = std::collections::HashMap::new();
    let mut open: Vec<Thread> = Vec::new();
    let mut sum = Summary::default();
    let gap = SPAN / count.max(1) as i64;
    let mut batch: Vec<Pending> = Vec::with_capacity(2000);

    for i in 0..count {
        let date = END - (count - 1 - i) as i64 * gap - (rng.next() % gap.max(1) as u64) as i64;
        let age_days = (END - date) / 86_400;
        let recent = i * 4 >= count * 3; // newest quarter
        let mid = format!("<{i}.{:x}@envoy-bench.test>", rng.next() & 0xffff_ffff);
        let roll = rng.f();
        let from: Person;
        let mut to = vec![me.clone()];
        let mut cc: Vec<Person> = Vec::new();
        let subject;
        let mut in_reply_to = String::new();
        let mut references = String::new();
        let mut text: Option<String> = None;
        let mut html: Option<String> = None;
        let mut attach = false;
        let mut from_me = false;
        let mut kind_box: Option<&str> = None;
        // Headers of bulk and automated mail (no randomness: the mailbox stays the same).
        let mut bulk = String::new();

        let reply_to_thread = roll < 0.33 && !open.is_empty();
        if reply_to_thread {
            let ti = open.len() - 1 - rng.below(open.len().min(60));
            let t = &mut open[ti];
            from_me = rng.chance(0.35);
            let author = t.people[rng.below(t.people.len())];
            if from_me {
                from = me.clone();
                to = t.people.iter().take(3).map(|&p| folks[p].clone()).collect();
            } else {
                from = folks[author].clone();
                to = vec![me.clone()];
                cc = t.people.iter().filter(|&&p| p != author).take(2).map(|&p| folks[p].clone()).collect();
            }
            subject = format!("Re: {}", t.subject);
            in_reply_to = t.last_mid.clone();
            t.refs.push(t.last_mid.clone());
            references = t.refs.join(" ");
            let mut body = paragraphs(&mut rng, 1, 4).join("\n\n");
            body.push_str(&format!("\n\nOn {}, {} wrote:\n", rfc2822(date - 3600), folks[author].name));
            for line in t.last_text.lines().take(30) {
                body.push_str("> ");
                body.push_str(line);
                body.push('\n');
            }
            body.push_str(&format!("\n-- \n{}\n", from.name));
            t.last_text = body.clone();
            t.last_mid = mid.clone();
            t.left -= 1;
            text = Some(body);
            attach = rng.chance(0.06);
            if t.left == 0 {
                open.remove(ti);
            }
            sum.replies += 1;
        } else if roll < 0.53 {
            // Bots and notifications: short text.
            let (n, e) = rng.pick(BOTS);
            from = Person { name: n.to_string(), email: e.to_string() };
            bulk = "Auto-Submitted: auto-generated\r\n".to_owned();
            let project = rng.pick(PROJECTS);
            subject = match rng.below(5) {
                0 => format!("[{}] Pull request #{}: {}", project.to_lowercase(), rng.range(100, 9999), rng.pick(TOPICS)),
                1 => format!("Build failed: {} main #{}", project, rng.range(1000, 99_999)),
                2 => format!("Invitation: {} sync @ {} 10:00", project, rng.pick(DAYS)),
                3 => format!("Your package {} is out for delivery", rng.range(100_000, 999_999)),
                _ => format!("New sign-in to your account from {}", rng.pick(&["Firefox on Linux", "Safari on macOS", "Edge on Windows"])),
            };
            text = Some(paragraphs(&mut rng, 1, 3).join("\n\n") + "\n\nYou are receiving this because you are subscribed to this thread.\n");
        } else if roll < 0.66 {
            let (n, e) = rng.pick(NEWSLETTERS);
            from = Person { name: n.to_string(), email: e.to_string() };
            let domain = e.split('@').nth(1).unwrap_or("example");
            bulk = format!("List-Unsubscribe: <https://{domain}/unsubscribe?u={i}>, <mailto:unsubscribe@{domain}>\r\nList-Id: {n} <list.{domain}>\r\n");
            subject = format!("{}: {}", n, sentence(&mut rng).trim_end_matches(['.', '?']));
            let h = newsletter_html(&mut rng, &subject, n);
            if rng.chance(0.5) {
                text = Some(paragraphs(&mut rng, 3, 8).join("\n\n"));
            }
            html = Some(h);
            if rng.chance(0.08) {
                kind_box = Some("Junk");
            }
        } else if roll < 0.70 {
            let (n, e) = rng.pick(SHOPS);
            from = Person { name: n.to_string(), email: e.to_string() };
            let order = rng.range(10_000, 9_999_999) as u32;
            subject = format!("Your receipt from {n} — order #{order}");
            html = Some(receipt_html(&mut rng, n, order));
            attach = rng.chance(0.5);
            kind_box = Some("Receipts");
        } else {
            // New conversation between people, sometimes started by me.
            from_me = rng.chance(0.2);
            let project = rng.pick(PROJECTS);
            let topic = rng.pick(TOPICS);
            subject = match rng.below(8) {
                0 => format!("{project} {topic}"),
                1 => format!("Notes from the {project} sync"),
                2 => format!("Can we move {}'s meeting?", rng.pick(DAYS)),
                3 => format!("Question about the {topic}"),
                4 => format!("Lunch {}?", rng.pick(DAYS)),
                5 => format!("Draft {topic} for review"),
                6 => format!("{project}: {topic} ({})", rng.range(2019, 2026)),
                _ => sentence(&mut rng).trim_end_matches(['.', '?']).to_string(),
            };
            let others: Vec<usize> = (0..rng.range(1, 4)).map(|_| rng.skewed(folks.len())).collect();
            if from_me {
                from = me.clone();
                to = others.iter().map(|&p| folks[p].clone()).collect();
            } else {
                from = folks[others[0]].clone();
                cc = others.iter().skip(1).map(|&p| folks[p].clone()).collect();
                // Every other group message goes to the team, you copied.
                if !cc.is_empty() && i % 2 == 0 {
                    let company = from.email.split('@').nth(1).unwrap_or("example").to_owned();
                    to = vec![Person { name: "Team".into(), email: format!("team@{company}") }];
                    cc.push(me.clone());
                }
            }
            let mut body = format!("Hi {},\n\n", if from_me { to[0].name.split(' ').next().unwrap_or("") } else { "there" });
            body.push_str(&paragraphs(&mut rng, 1, 6).join("\n\n"));
            body.push_str(&format!("\n\nBest,\n{}\n", from.name.split(' ').next().unwrap_or("")));
            attach = rng.chance(0.12);
            if rng.chance(0.55) {
                open.push(Thread {
                    last_mid: mid.clone(),
                    refs: vec![],
                    subject: subject.clone(),
                    people: others,
                    left: rng.range(1, 11),
                    last_text: body.clone(),
                });
                if open.len() > 400 {
                    open.remove(0);
                }
            }
            text = Some(body);
        }

        let mailbox: &str = if from_me {
            "Sent"
        } else if let Some(b) = kind_box {
            b
        } else {
            let r = rng.f();
            if recent {
                if r < 0.62 { "INBOX" } else if r < 0.90 { "Archive" } else if r < 0.94 { "Work/Clients" } else if r < 0.97 { "Trash" } else { "Junk" }
            } else if r < 0.05 {
                "INBOX"
            } else if r < 0.88 {
                "Archive"
            } else if r < 0.93 {
                "Work/Clients"
            } else if r < 0.97 {
                "Trash"
            } else {
                "Junk"
            }
        };
        let mailbox = if rng.chance(0.002) { "Drafts" } else { mailbox };

        let mut fl = 0u32;
        // Recent inbox mail is often unread, old inbox mail sometimes, ignored newsletters a lot.
        let p_unread = match mailbox {
            "Sent" | "Drafts" => 0.0,
            "INBOX" if age_days < 30 => 0.35,
            "INBOX" => 0.10,
            _ if html.is_some() && kind_box.is_none() => 0.25,
            _ => 0.004,
        };
        let unread = rng.chance(p_unread);
        if !unread {
            fl |= flags::SEEN;
        }
        if rng.chance(0.03) {
            fl |= flags::FLAGGED;
        }
        if from_me && !in_reply_to.is_empty() {
            fl |= flags::SEEN;
        }

        // Raw RFC 822.
        let mut raw = String::with_capacity(4096);
        raw.push_str(&format!("From: {}\r\n", addr(&from.name, &from.email)));
        raw.push_str(&format!("To: {}\r\n", to.iter().map(|p| addr(&p.name, &p.email)).collect::<Vec<_>>().join(", ")));
        if !cc.is_empty() {
            raw.push_str(&format!("Cc: {}\r\n", cc.iter().map(|p| addr(&p.name, &p.email)).collect::<Vec<_>>().join(", ")));
        }
        raw.push_str(&format!("Subject: {subject}\r\nDate: {}\r\nMessage-ID: {mid}\r\n", rfc2822(date)));
        raw.push_str(&bulk);
        if !in_reply_to.is_empty() {
            raw.push_str(&format!("In-Reply-To: {in_reply_to}\r\nReferences: {references}\r\n"));
        }
        raw.push_str("MIME-Version: 1.0\r\n");
        let body_part = match (&text, &html) {
            (Some(t), Some(h)) => format!(
                "Content-Type: multipart/alternative; boundary=\"alt{i}\"\r\n\r\n--alt{i}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{t}\r\n--alt{i}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{h}\r\n--alt{i}--\r\n"
            ),
            (None, Some(h)) => format!("Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{h}\r\n"),
            (Some(t), None) => format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{t}\r\n"),
            (None, None) => "Content-Type: text/plain; charset=utf-8\r\n\r\n\r\n".to_string(),
        };
        if attach {
            let b = format!("mix{i}");
            raw.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n--{b}\r\n{body_part}"));
            for _ in 0..rng.range(1, 3) {
                raw.push_str(&attachment_part(&mut rng, &b));
            }
            raw.push_str(&format!("--{b}--\r\n"));
            sum.with_attachments += 1;
        } else {
            raw.push_str(&body_part);
        }
        if html.is_some() {
            sum.html += 1;
        }
        if unread {
            sum.unread += 1;
        }
        let uid = uids.entry(mailbox).or_insert(0);
        *uid += 1;
        let c = counts.entry(mailbox).or_insert((0, 0));
        c.1 += 1;
        if unread {
            c.0 += 1;
        }
        sum.bytes_raw += raw.len();
        batch.push(Pending { mailbox: mailbox.to_string(), raw: raw.into_bytes(), uid: *uid, flags: fl, date });
        if batch.len() >= 2000 || i + 1 == count {
            write_batch(&store, &mut batch)?;
        }
        sum.messages += 1;
    }
    for (name, (unread, total)) in counts {
        store.set_counts(ACCOUNT, name, unread, total)?;
    }
    Ok(sum)
}

struct Pending {
    mailbox: String,
    raw: Vec<u8>,
    uid: u32,
    flags: u32,
    date: i64,
}

fn write_batch(store: &Store, batch: &mut Vec<Pending>) -> Result<()> {
    store.transaction(|st| {
        for m in batch.iter() {
            let parsed = mime::parse(&m.raw, false).context("synthetic message didn't parse")?;
            let id = st.upsert_header(ACCOUNT, &m.mailbox, m.uid, m.flags, m.raw.len() as u32, m.date, &parsed)?;
            st.set_body(id, &m.raw, &parsed)?;
        }
        Ok(())
    })?;
    batch.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(db: &Path) -> (i64, String) {
        let c = rusqlite::Connection::open(db).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        let sample: String = c
            .query_row("SELECT group_concat(subject || from_email || date || flags || mailbox, '|') FROM (SELECT * FROM messages ORDER BY id LIMIT 200)", [], |r| r.get(0))
            .unwrap();
        (n, sample)
    }

    #[test]
    fn vocabulary_is_as_large_as_claimed() {
        let v = vocab();
        assert_eq!(v.len(), VOCAB_SIZE);
        let distinct: std::collections::HashSet<&String> = v.iter().collect();
        assert_eq!(distinct.len(), VOCAB_SIZE, "every rank is a different token");
        assert!(content_rank(CONTENT.len() - 1) < VOCAB_SIZE);
        for (i, w) in CONTENT.iter().enumerate() {
            assert_eq!(&v[content_rank(i)], w);
        }
        for w in v {
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w:?} would split into several FTS tokens");
        }
    }

    #[test]
    fn same_seed_same_mailbox() {
        let dir = std::env::temp_dir().join(format!("envoy-bench-det-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        generate(&a, 1500, 7).unwrap();
        generate(&b, 1500, 7).unwrap();
        assert_eq!(digest(&a.join("mail.sqlite")), digest(&b.join("mail.sqlite")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mailbox_has_the_promised_shape() {
        let dir = std::env::temp_dir().join(format!("envoy-bench-shape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let n = 6000;
        let s = generate(&dir, n, 1).unwrap();
        assert_eq!(s.messages, n);
        let frac = |x: usize| x as f64 / n as f64;
        assert!((0.10..0.25).contains(&frac(s.html)), "{} HTML", s.html);
        assert!((0.04..0.15).contains(&frac(s.with_attachments)), "{} with attachments", s.with_attachments);
        assert!((0.25..0.40).contains(&frac(s.replies)), "{} replies", s.replies);
        assert!(s.unread > 50, "{} unread", s.unread);

        // Document frequencies: what the search queries' "common / medium / rare" rely on.
        let c = rusqlite::Connection::open(dir.join("mail.sqlite")).unwrap();
        let df = |word: &str| -> f64 {
            let k: i64 = c.query_row("SELECT COUNT(*) FROM fts WHERE fts MATCH ?1", [format!("body:{word}")], |r| r.get(0)).unwrap();
            k as f64 / n as f64
        };
        let (meeting, invoice, pancakes) = (df("meeting"), df("invoice"), df("pancakes"));
        eprintln!("document frequency: meeting {:.1}%, invoice {:.1}%, pancakes {:.2}%", meeting * 100.0, invoice * 100.0, pancakes * 100.0);
        assert!((0.15..0.40).contains(&meeting), "meeting in {:.1}% of messages", meeting * 100.0);
        assert!((0.01..0.08).contains(&invoice), "invoice in {:.1}% of messages", invoice * 100.0);
        assert!(pancakes < 0.005, "pancakes in {:.2}% of messages", pancakes * 100.0);
        let distinct: i64 = {
            c.execute_batch("CREATE VIRTUAL TABLE terms USING fts5vocab(fts, 'row')").unwrap();
            c.query_row("SELECT COUNT(*) FROM terms", [], |r| r.get(0)).unwrap()
        };
        eprintln!("{distinct} distinct terms indexed in {n} messages");
        assert!(distinct > 10_000, "only {distinct} distinct terms indexed");

        let st = Store::open(&dir.join("mail.sqlite")).unwrap();
        let me: std::collections::HashSet<String> = [ME.to_string()].into();
        let inbox = envoy_core::model::View::Role { account: None, role: Role::Inbox };
        for q in ["meeting", "from:alice", "is:unread", "has:attachment"] {
            let rows = st.list(&inbox, &envoy_core::store::Query::parse(q), true, 3000, &me).unwrap();
            assert!(!rows.is_empty(), "{q} found nothing");
        }
        // Every tab of the split inbox has mail, and the engine's classifier finds nothing to redo.
        for c in envoy_core::model::Category::ALL {
            let rows = st.list(&inbox, &envoy_core::store::Query::in_category(c), true, 3000, &me).unwrap();
            eprintln!("inbox {}: {} conversations", c.label(), rows.len());
            assert!(!rows.is_empty(), "no {} in the inbox", c.label());
        }
        st.set_classifier(envoy_core::category::Classifier::new(Vec::new(), [ME.to_owned()])).unwrap();
        assert!(!st.recategorizing().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
