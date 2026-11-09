//! The split inbox: every conversation is Important, Other, Newsletters or Notifications,
//! decided here, on this computer, from its headers and the user's rules. Nothing about it ever
//! reaches the server (docs/NORTH_STAR.md, law 2).
//!
//! A message's category comes from, in order:
//!
//! 1. **Rules** (`inbox.rules` in settings.json): the first that matches its sender (`from`) or
//!    mailing list (`list`) decides. Rules beat everything below.
//! 2. **Heuristics**, from what's known when it's stored ([`verdict`]): you sent it → Important;
//!    sent by a machine (`Auto-Submitted`, a notification system's headers, a notifications or
//!    no-reply address, a receipt or alert) → Notifications; bulk (`List-Unsubscribe`, `List-Id`,
//!    `Precedence: bulk`, a newsletter platform) → Newsletters, except a mailing list post from
//!    someone you've written to → Other; from someone you've written to, marked important by
//!    Gmail, or sent to you directly → Important; anything else (copied, sent to a group) → Other.
//!
//! A **conversation** takes the strongest category of its messages ([`conversation`]): a person
//! beats a machine, and you beat anyone, so a newsletter you replied to is Important. A message a
//! rule placed beats any a heuristic placed, so a rule always moves its sender's conversations;
//! and a correction of one conversation (an override) beats both.
//!
//! The category is computed when a message is stored and kept with it (`messages.category`,
//! indexed), so a category's list reads its index like any folder's.

use std::collections::HashSet;

use serde_json::{Value, json};

pub use crate::model::Category;

/// Bumped when the heuristics change: stored categories are worked out again, in the background.
/// 2: whether mail is to you is judged from its recipients with your addresses as they are now,
/// no longer stored as a signal.
pub const HEURISTICS: u32 = 2;

/// What a message's headers say about it ([`crate::mime::BulkHeaders`]), stored as
/// `messages.signals` so a rule change never needs the headers again. None of it depends on your
/// addresses (whether it's to you is worked out when it's judged, from its recipients), so your
/// addresses changing never leaves a stale signal.
pub mod signals {
    /// The headers were read (the sync asked for the bulk headers, or the whole message was
    /// downloaded). Messages cached by an Envoy from before the split lack it until their body
    /// is downloaded; they're judged by what the cache has.
    pub const KNOWN: u32 = 1;
    pub const LIST_UNSUBSCRIBE: u32 = 1 << 1;
    pub const LIST_ID: u32 = 1 << 2;
    pub const LIST_POST: u32 = 1 << 3;
    /// `Precedence: bulk` or `junk`.
    pub const BULK: u32 = 1 << 4;
    /// `Precedence: list`.
    pub const LIST: u32 = 1 << 5;
    /// `Auto-Submitted`, other than `no`.
    pub const AUTO: u32 = 1 << 6;
    /// A notification system's own headers (`X-GitHub-Reason`, …).
    pub const PLATFORM: u32 = 1 << 7;
}

/// [`signals`] for a parsed message.
pub fn signals_of(p: &crate::mime::Parsed) -> u32 {
    use signals::*;
    let b = &p.bulk_headers;
    let mut s = KNOWN;
    let set = |s: &mut u32, on: bool, bit: u32| {
        if on {
            *s |= bit;
        }
    };
    set(&mut s, b.list_unsubscribe, LIST_UNSUBSCRIBE);
    set(&mut s, !b.list_id.is_empty(), LIST_ID);
    set(&mut s, b.list_post, LIST_POST);
    set(&mut s, matches!(b.precedence.as_str(), "bulk" | "junk"), BULK);
    set(&mut s, b.precedence == "list", LIST);
    set(&mut s, !b.auto_submitted.is_empty() && b.auto_submitted != "no", AUTO);
    set(&mut s, b.platform, PLATFORM);
    s
}

/// What's known about one message when it's judged.
#[derive(Clone, Debug, Default)]
pub struct Facts<'a> {
    /// The sender's address.
    pub from: &'a str,
    pub subject: &'a str,
    pub signals: u32,
    pub list_id: &'a str,
    /// You sent it (from one of your addresses, or in a Sent folder).
    pub from_me: bool,
    /// You've written to its sender.
    pub written_to: bool,
    /// Gmail has it in Important.
    pub gmail_important: bool,
    /// One of your addresses (as they are now) is in `To`, not only `Cc`; for a message cached
    /// before the split, which doesn't know which recipients were in `To`, among its recipients.
    pub to_me: bool,
}

/// Why a message (or conversation) is where it is, to tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// Moved there by hand.
    Moved,
    /// The rule at this index of `inbox.rules`.
    Rule(usize, Rule),
    FromYou,
    AutoSubmitted,
    Platform,
    NotificationSender,
    NoReply,
    Receipt,
    Unsubscribe,
    MailingList,
    BulkPrecedence,
    NewsletterSender,
    /// A mailing list post from someone you've written to.
    ListFromSomeoneYouKnow,
    WrittenTo,
    GmailImportant,
    ToYou,
    /// None of the above: copied, or sent to a group.
    Rest,
}

impl Why {
    /// A sentence for the user ("Has an unsubscribe link").
    pub fn describe(&self) -> String {
        match self {
            Why::Moved => "You moved this conversation here".into(),
            Why::Rule(_, r) => format!("Your rule: {}", r.text()),
            Why::FromYou => "You're part of this conversation".into(),
            Why::AutoSubmitted => "Sent automatically (Auto-Submitted)".into(),
            Why::Platform => "A notification from a tool you use".into(),
            Why::NotificationSender => "From a notifications address".into(),
            Why::NoReply => "From a no-reply address".into(),
            Why::Receipt => "Looks like a receipt, booking or alert".into(),
            Why::Unsubscribe => "Has an unsubscribe link".into(),
            Why::MailingList => "Sent to a mailing list".into(),
            Why::BulkPrecedence => "Sent in bulk".into(),
            Why::NewsletterSender => "From a newsletter address".into(),
            Why::ListFromSomeoneYouKnow => "A mailing list post from someone you've written to".into(),
            Why::WrittenTo => "From someone you've written to".into(),
            Why::GmailImportant => "Marked important by Gmail".into(),
            Why::ToYou => "Sent directly to you".into(),
            Why::Rest => "You're copied, or it went to a group".into(),
        }
    }

    pub fn by_rule(&self) -> bool {
        matches!(self, Why::Rule(..))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub category: Category,
    pub why: Why,
}

fn v(category: Category, why: Why) -> Verdict {
    Verdict { category, why }
}

/// A user rule: mail whose sender matches `from` and whose mailing list matches `list` (each
/// only if given) goes to `category`. In settings.json: `{ "from": "*@github.com", "category":
/// "notifications" }`; as text (Settings, the README): `from:*@github.com notifications`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub from: String,
    pub list: String,
    pub category: Category,
}

impl Rule {
    /// A rule for one sender's address, as a correction makes.
    pub fn sender(address: &str, category: Category) -> Rule {
        Rule { from: address.trim().to_lowercase(), list: String::new(), category }
    }

    pub fn matches(&self, from: &str, list_id: &str) -> bool {
        (self.from.is_empty() || address_matches(&self.from, &from.to_lowercase())) && (self.list.is_empty() || (!list_id.is_empty() && list_matches(&self.list, &list_id.to_lowercase())))
    }

    /// Exactly one address and nothing else: changing it touches only that sender's mail.
    pub fn sender_only(&self) -> Option<&str> {
        (self.list.is_empty() && !self.from.contains('*') && !self.from.starts_with('@') && self.from.contains('@')).then_some(self.from.as_str())
    }

    /// As settings.json holds it: no empty fields.
    pub fn to_json(&self) -> Value {
        let mut rule = serde_json::Map::new();
        if !self.from.is_empty() {
            rule.insert("from".into(), self.from.clone().into());
        }
        if !self.list.is_empty() {
            rule.insert("list".into(), self.list.clone().into());
        }
        rule.insert("category".into(), self.category.as_str().into());
        Value::Object(rule)
    }

    /// Read from [`Rule::to_json`]'s form, as written (not checked, not lowercased).
    fn from_json(v: &Value) -> Result<Rule, String> {
        let o = v.as_object().ok_or("not a rule ({ \"from\": …, \"category\": … })")?;
        let text = |key: &str| match o.get(key) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("{key} must be text")),
        };
        let category = o.get("category").and_then(Value::as_str).ok_or("no category")?;
        let category = Category::parse(category).ok_or_else(|| format!("unknown category \"{category}\""))?;
        Ok(Rule { from: text("from")?, list: text("list")?, category })
    }

    /// `from:… list:… category`.
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        if !self.from.is_empty() {
            parts.push(format!("from:{}", self.from));
        }
        if !self.list.is_empty() {
            parts.push(format!("list:{}", self.list));
        }
        parts.push(self.category.as_str().to_owned());
        parts.join(" ")
    }

    /// Reads [`Rule::text`]'s form: `from:` and `list:` values, and a category, in any order.
    pub fn parse_text(line: &str) -> Result<Rule, String> {
        let mut rule = Rule { from: String::new(), list: String::new(), category: Category::Other };
        let mut category = None;
        for tok in line.split_whitespace() {
            let lower = tok.to_lowercase();
            if let Some(v) = lower.strip_prefix("from:") {
                rule.from = v.to_owned();
            } else if let Some(v) = lower.strip_prefix("list:") {
                rule.list = v.to_owned();
            } else if let Some(c) = Category::parse(&lower) {
                category = Some(c);
            } else {
                return Err(format!("\u{201c}{tok}\u{201d} isn't from:, list: or a category"));
            }
        }
        rule.category = category.ok_or("no category (important, other, newsletters or notifications)")?;
        rule.check()?;
        Ok(rule)
    }

    /// Lowercased and trimmed, if it can match anything.
    fn check(&self) -> Result<(), String> {
        if self.from.is_empty() && self.list.is_empty() {
            return Err("a rule needs from: or list:".into());
        }
        if !self.from.is_empty() && !pattern_ok(&self.from, true) {
            return Err(format!("from:{} isn't an address, a domain or a pattern like *@example.com", self.from));
        }
        if !self.list.is_empty() && !pattern_ok(&self.list, false) {
            return Err(format!("list:{} isn't a list id or a pattern", self.list));
        }
        Ok(())
    }
}

/// Reads `inbox.rules` (a JSON list of rule objects): the rules that make sense, lowercased,
/// and a problem for each entry that doesn't.
pub fn parse_rules(v: &serde_json::Value) -> (Vec<Rule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut problems = Vec::new();
    let Some(items) = v.as_array() else {
        return (rules, vec!["inbox.rules must be a list of rules".into()]);
    };
    for (i, item) in items.iter().enumerate() {
        match Rule::from_json(item) {
            Ok(mut r) => {
                r.from = r.from.trim().to_lowercase();
                r.list = r.list.trim().to_lowercase();
                match r.check() {
                    Ok(()) => rules.push(r),
                    Err(e) => problems.push(format!("rule {}: {e}", i + 1)),
                }
            }
            Err(e) => problems.push(format!("rule {}: {e}", i + 1)),
        }
    }
    (rules, problems)
}

/// A rule's pattern: something with a letter or digit, of the characters addresses and list ids
/// are made of (and `*`).
fn pattern_ok(p: &str, address: bool) -> bool {
    let allowed = |c: char| c.is_alphanumeric() || "*.-_+@".contains(c) || (address && "!#$%&'/=?^`{|}~".contains(c));
    p.chars().any(|c| c.is_alphanumeric()) && p.chars().all(allowed) && (p.contains('*') || p.contains('.'))
}

/// `pattern` against a lowercase address: `*` globs, `user@domain` exactly, `domain` or
/// `@domain` that domain and its subdomains.
fn address_matches(pattern: &str, address: &str) -> bool {
    if pattern.contains('*') {
        return glob(pattern, address);
    }
    if let Some(domain) = pattern.strip_prefix('@') {
        return domain_matches(domain, address.rsplit_once('@').map_or("", |(_, d)| d));
    }
    if pattern.contains('@') {
        return pattern == address;
    }
    domain_matches(pattern, address.rsplit_once('@').map_or("", |(_, d)| d))
}

/// A list id: `*` globs; otherwise the id, or one ending in `.` and it.
fn list_matches(pattern: &str, id: &str) -> bool {
    if pattern.contains('*') { glob(pattern, id) } else { domain_matches(pattern, id) }
}

fn domain_matches(domain: &str, host: &str) -> bool {
    !domain.is_empty() && (host == domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.')))
}

/// `*` matches any run of characters.
fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Local parts (before `@`, split at `-`, `_`, `.` and `+`) only machines send from.
const NOTIFICATION_NAMES: &[&str] = &[
    "notifications", "notification", "notify", "notifier", "alerts", "alert", "receipts", "receipt", "invoice", "invoices", "billing", "orders",
    "order", "shipping", "shipment", "tracking", "security", "verify", "verification", "account", "accounts", "calendar", "reminder", "reminders",
    "automated", "bounce", "bounces", "mailer-daemon", "mailerdaemon", "postmaster", "ci", "builds", "jira", "confluence", "bot",
];
/// Local parts newsletters and promotions come from.
const NEWSLETTER_NAMES: &[&str] = &["newsletter", "newsletters", "news", "digest", "deals", "offers", "promo", "promos", "promotions", "marketing", "weekly"];
/// Newsletter platforms: mail from these domains (or their subdomains) is a newsletter.
const NEWSLETTER_DOMAINS: &[&str] = &["substack.com", "beehiiv.com", "buttondown.email", "ghost.io", "mailchimpapp.com", "convertkit.com", "mailerlite.com", "getrevue.co"];
/// Subjects of receipts, bookings and alerts (lowercase).
const RECEIPT_SUBJECTS: &[&str] = &[
    "receipt", "invoice", "your order", "order #", "order confirmation", "has shipped", "out for delivery", "was delivered", "payment received",
    "payment confirmation", "booking confirmation", "your booking", "your reservation", "password reset", "reset your password", "verification code",
    "verify your", "security alert", "new sign-in", "sign-in attempt", "login attempt", "statement is ready", "your bill", "appointment reminder",
];

fn local_part(address: &str) -> String {
    let local = address.rsplit_once('@').map_or(address, |(l, _)| l).to_lowercase();
    local.split('+').next().unwrap_or("").to_owned()
}

fn domain_of(address: &str) -> String {
    address.rsplit_once('@').map_or("", |(_, d)| d).to_lowercase()
}

fn names(local: &str) -> impl Iterator<Item = &str> {
    local.split(['-', '_', '.']).filter(|t| !t.is_empty())
}

fn no_reply(address: &str) -> bool {
    let squashed: String = local_part(address).chars().filter(|c| c.is_alphanumeric()).collect();
    squashed.contains("noreply") || squashed.contains("donotreply")
}

fn notification_sender(address: &str) -> bool {
    let local = local_part(address);
    NOTIFICATION_NAMES.contains(&local.as_str()) || names(&local).any(|t| NOTIFICATION_NAMES.contains(&t))
}

fn newsletter_sender(address: &str) -> bool {
    let local = local_part(address);
    let domain = domain_of(address);
    names(&local).any(|t| NEWSLETTER_NAMES.contains(&t)) || NEWSLETTER_DOMAINS.iter().any(|d| domain_matches(d, &domain))
}

fn receipt_subject(subject: &str) -> bool {
    let s = subject.trim_start().to_lowercase();
    // A reply or forward is a person writing about one.
    let reply = ["re:", "fwd:", "fw:", "aw:", "sv:", "tr:"].iter().any(|p| s.starts_with(p));
    !reply && RECEIPT_SUBJECTS.iter().any(|p| s.contains(p))
}

/// The heuristics' verdict on one message (rules aside: see [`Classifier::verdict`]).
pub fn verdict(f: &Facts) -> Verdict {
    use signals::*;
    let has = |bit: u32| f.signals & bit != 0;
    if f.from_me {
        return v(Category::Important, Why::FromYou);
    }
    if has(AUTO) {
        return v(Category::Notifications, Why::AutoSubmitted);
    }
    if has(PLATFORM) {
        return v(Category::Notifications, Why::Platform);
    }
    if notification_sender(f.from) {
        return v(Category::Notifications, Why::NotificationSender);
    }
    let list = has(LIST_POST) || has(LIST);
    if has(LIST_UNSUBSCRIBE) || has(LIST_ID) || has(BULK) || has(LIST) || newsletter_sender(f.from) {
        if list && f.written_to {
            return v(Category::Other, Why::ListFromSomeoneYouKnow);
        }
        let why = if has(LIST_UNSUBSCRIBE) {
            Why::Unsubscribe
        } else if has(LIST_ID) || has(LIST) {
            Why::MailingList
        } else if has(BULK) {
            Why::BulkPrecedence
        } else {
            Why::NewsletterSender
        };
        return v(Category::Newsletters, why);
    }
    if no_reply(f.from) {
        return v(Category::Notifications, Why::NoReply);
    }
    if !f.written_to && receipt_subject(f.subject) {
        return v(Category::Notifications, Why::Receipt);
    }
    if f.written_to {
        return v(Category::Important, Why::WrittenTo);
    }
    if f.gmail_important {
        return v(Category::Important, Why::GmailImportant);
    }
    if f.to_me {
        return v(Category::Important, Why::ToYou);
    }
    v(Category::Other, Why::Rest)
}

/// A conversation's verdict from its messages': a correction beats everything; else the
/// strongest category a rule gave any of them; else the strongest the heuristics gave. Ties go
/// to the first (the newest, as the store reads them). Empty without messages or a correction.
pub fn conversation(messages: &[Verdict], moved: Option<Category>) -> Option<Verdict> {
    if let Some(c) = moved {
        return Some(v(c, Why::Moved));
    }
    let strongest = |by_rule: bool| messages.iter().filter(|m| m.why.by_rule() == by_rule).fold(None::<&Verdict>, |best, m| match best {
        Some(b) if b.category.strength() >= m.category.strength() => Some(b),
        _ => Some(m),
    });
    strongest(true).or_else(|| strongest(false)).cloned()
}

/// What categories are worked out with: the user's rules and addresses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Classifier {
    pub rules: Vec<Rule>,
    /// Your addresses, lowercase.
    pub me: HashSet<String>,
}

impl Classifier {
    pub fn new(rules: Vec<Rule>, me: impl IntoIterator<Item = String>) -> Classifier {
        Classifier { rules, me: me.into_iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).collect() }
    }

    /// From settings (`inbox.rules`) and the accounts' addresses.
    pub fn from_settings(settings: &crate::settings::Settings, me: impl IntoIterator<Item = String>) -> Classifier {
        Classifier::new(parse_rules(&settings.value("inbox.rules")).0, me)
    }

    /// The first rule that matches, then the heuristics.
    pub fn verdict(&self, f: &Facts) -> Verdict {
        match self.rules.iter().enumerate().find(|(_, r)| r.matches(f.from, f.list_id)) {
            Some((i, r)) => v(r.category, Why::Rule(i, r.clone())),
            None => verdict(f),
        }
    }

    pub fn is_me(&self, address: &str) -> bool {
        self.me.contains(&address.trim().to_lowercase())
    }

    /// Everything the stored categories depend on, to tell whether they're current: the
    /// heuristics' version, your addresses and the rules.
    pub fn fingerprint(&self) -> String {
        let mut me: Vec<&str> = self.me.iter().map(String::as_str).collect();
        me.sort_unstable();
        json!({ "heuristics": HEURISTICS, "me": me, "rules": self.rules.iter().map(Rule::to_json).collect::<Vec<_>>() }).to_string()
    }

    /// Read back from [`Classifier::fingerprint`]; None for anything else (an old or empty one).
    pub fn from_fingerprint(s: &str) -> Option<Classifier> {
        let v: Value = serde_json::from_str(s).ok()?;
        if v.get("heuristics")?.as_u64()? != u64::from(HEURISTICS) {
            return None;
        }
        let me = v.get("me")?.as_array()?.iter().map(|a| a.as_str().map(str::to_owned)).collect::<Option<Vec<String>>>()?;
        let rules = v.get("rules")?.as_array()?.iter().map(|r| Rule::from_json(r).ok()).collect::<Option<Vec<Rule>>>()?;
        Some(Classifier::new(rules, me))
    }

    /// When going from `self` to `next` changes only rules for single addresses (as corrections
    /// add): those addresses, whose mail is all that moves. None when anything else changed, and
    /// every conversation has to be looked at again.
    pub fn senders_changed(&self, next: &Classifier) -> Option<Vec<String>> {
        if self.me != next.me {
            return None;
        }
        // A few rules each: lists, not sets.
        let (old, new) = (&self.rules, &next.rules);
        let mut senders = Vec::new();
        for r in old.iter().filter(|r| !new.contains(r)).chain(new.iter().filter(|r| !old.contains(r))) {
            senders.push(r.sender_only()?.to_owned());
        }
        // A rule moving up or down only matters against another rule for the same mail; for
        // single-address rules that's the same address, among those already collected or not.
        let kept_old: Vec<&Rule> = old.iter().filter(|r| new.contains(r)).collect();
        let kept_new: Vec<&Rule> = new.iter().filter(|r| old.contains(r)).collect();
        if kept_old != kept_new {
            for r in kept_new {
                senders.push(r.sender_only()?.to_owned());
            }
        }
        senders.sort();
        senders.dedup();
        Some(senders)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mime;

    fn parse(headers: &str) -> mime::Parsed {
        mime::parse(format!("{headers}\r\n\r\nbody\r\n").as_bytes(), true).unwrap()
    }

    fn me() -> HashSet<String> {
        ["me@example.com".to_owned()].into()
    }

    /// The heuristics' verdict on a message with these headers, as stored would see it.
    fn judge(headers: &str, written_to: bool, gmail_important: bool) -> Verdict {
        let p = parse(headers);
        let me = me();
        let f = Facts {
            from: &p.from.email,
            subject: &p.subject,
            signals: signals_of(&p),
            list_id: &p.bulk_headers.list_id,
            from_me: me.contains(&p.from.email.to_lowercase()),
            written_to,
            gmail_important,
            to_me: p.to.iter().any(|a| me.contains(&a.email.to_lowercase())),
        };
        verdict(&f)
    }

    #[test]
    fn a_newsletter_is_a_newsletter() {
        let h = "From: Northwind Weekly <hello@northwind.example>\r\nTo: me@example.com\r\nSubject: This week: SQLite everywhere\r\nList-Unsubscribe: <https://northwind.example/u/123>, <mailto:u@northwind.example>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click";
        assert_eq!(judge(h, false, false), v(Category::Newsletters, Why::Unsubscribe));
        // Even from someone you've written to (you once replied to the author), and to you.
        assert_eq!(judge(h, true, false).category, Category::Newsletters);
        // Precedence: bulk alone, and a newsletter platform alone.
        assert_eq!(judge("From: deals@shop.example\r\nTo: me@example.com\r\nSubject: 30% off\r\nPrecedence: bulk", false, false), v(Category::Newsletters, Why::BulkPrecedence));
        assert_eq!(judge("From: Ana <ana@substack.com>\r\nTo: me@example.com\r\nSubject: On gardens", false, false), v(Category::Newsletters, Why::NewsletterSender));
        // Marketing from a no-reply address, with an unsubscribe link: a newsletter, not a notification.
        assert_eq!(judge("From: Brand <no-reply@brand.example>\r\nTo: me@example.com\r\nSubject: New arrivals\r\nList-Unsubscribe: <https://brand.example/u>", false, false).category, Category::Newsletters);
    }

    #[test]
    fn a_github_notification_is_a_notification() {
        let h = "From: Jordan Lee <notifications@github.com>\r\nTo: anomalyco/envoy <envoy@noreply.github.com>\r\nCc: Me <me@example.com>, Review requested <review_requested@noreply.github.com>\r\nSubject: Re: [anomalyco/envoy] Faster thread loading (PR #42)\r\nList-ID: anomalyco/envoy <envoy.anomalyco.github.com>\r\nList-Unsubscribe: <mailto:unsub+abc@reply.github.com>\r\nX-GitHub-Reason: review_requested\r\nPrecedence: list";
        let got = judge(h, false, false);
        assert_eq!(got.category, Category::Notifications, "{got:?}");
        assert_eq!(parse(h).bulk_headers.list_id, "envoy.anomalyco.github.com");
        // The platform header alone does it, whoever it's from.
        assert_eq!(judge("From: Bo <bo@example.org>\r\nTo: me@example.com\r\nSubject: hi\r\nX-GitLab-Project: envoy", false, false), v(Category::Notifications, Why::Platform));
        assert_eq!(judge("From: jira@acme.atlassian.net\r\nTo: me@example.com\r\nSubject: [JIRA] ENG-1", true, false).why, Why::NotificationSender);
        assert_eq!(judge("From: Calendar <calendar-notification@google.com>\r\nTo: me@example.com\r\nSubject: Invitation: sync", false, false).category, Category::Notifications);
        assert_eq!(judge("From: Out of office <bo@example.org>\r\nTo: me@example.com\r\nSubject: Away\r\nAuto-Submitted: auto-replied", true, false), v(Category::Notifications, Why::AutoSubmitted));
        // `Auto-Submitted: no` is a person.
        assert_eq!(judge("From: Bo <bo@example.org>\r\nTo: me@example.com\r\nSubject: hi\r\nAuto-Submitted: no", false, false).category, Category::Important);
    }

    #[test]
    fn receipts_and_alerts_are_notifications() {
        assert_eq!(judge("From: Stripe <receipts@stripe.com>\r\nTo: me@example.com\r\nSubject: Your receipt from Fly.io #2291-4410", false, false), v(Category::Notifications, Why::NotificationSender));
        assert_eq!(judge("From: Lufthansa <booking@lufthansa.com>\r\nTo: me@example.com\r\nSubject: Your booking confirmation: SFO - MUC", false, false), v(Category::Notifications, Why::Receipt));
        assert_eq!(judge("From: Shop <hi@shop.example>\r\nTo: me@example.com\r\nSubject: Your order #1234 has shipped", false, false).category, Category::Notifications);
        assert_eq!(judge("From: Docs <comments-noreply@docs.google.com>\r\nTo: me@example.com\r\nSubject: Ana replied", false, false), v(Category::Notifications, Why::NoReply));
        // A person replying about an invoice is a person.
        assert_eq!(judge("From: Bo <bo@example.org>\r\nTo: me@example.com\r\nSubject: Re: invoice for March", false, false), v(Category::Important, Why::ToYou));
        // And one you write to, about anything.
        assert_eq!(judge("From: Bo <bo@example.org>\r\nTo: me@example.com\r\nSubject: Your booking confirmation", true, false), v(Category::Important, Why::WrittenTo));
    }

    #[test]
    fn people_are_important_when_its_for_you_and_other_when_youre_copied() {
        // Direct to you.
        assert_eq!(judge("From: Sam Ortiz <sam@ortiz.family>\r\nTo: Me <me@example.com>\r\nSubject: Dinner Saturday?", false, false), v(Category::Important, Why::ToYou));
        // Only copied: Other.
        let cc = "From: Jordan <jordan@northwind.dev>\r\nTo: team@northwind.dev\r\nCc: me@example.com\r\nSubject: Offsite agenda";
        assert_eq!(judge(cc, false, false), v(Category::Other, Why::Rest));
        // Copied, but Gmail says it's important, or you write to them: Important.
        assert_eq!(judge(cc, false, true), v(Category::Important, Why::GmailImportant));
        assert_eq!(judge(cc, true, false), v(Category::Important, Why::WrittenTo));
        // Sent by you.
        assert_eq!(judge("From: Me <ME@example.com>\r\nTo: sam@ortiz.family\r\nSubject: Re: Dinner", false, false), v(Category::Important, Why::FromYou));
        // A mailing list post: from a stranger a newsletter, from someone you write to Other.
        let list = "From: Priya <priya@acme.co>\r\nTo: eng@lists.acme.co\r\nSubject: Q3 roadmap\r\nList-Id: Engineering <eng.lists.acme.co>\r\nList-Post: <mailto:eng@lists.acme.co>\r\nList-Unsubscribe: <mailto:eng-unsub@lists.acme.co>";
        assert_eq!(judge(list, false, false).category, Category::Newsletters);
        assert_eq!(judge(list, true, false), v(Category::Other, Why::ListFromSomeoneYouKnow));
    }

    #[test]
    fn messages_cached_before_the_split_are_judged_by_what_the_cache_has() {
        // No headers read: whether it's to you comes from its recipients (To and Cc together).
        let f = Facts { from: "bo@example.org", subject: "hi", signals: 0, to_me: true, ..Facts::default() };
        assert_eq!(verdict(&f), v(Category::Important, Why::ToYou));
        let f = Facts { from: "bo@example.org", subject: "hi", signals: 0, ..Facts::default() };
        assert_eq!(verdict(&f), v(Category::Other, Why::Rest));
        let f = Facts { from: "noreply@bank.example", subject: "Statement", signals: 0, to_me: true, ..Facts::default() };
        assert_eq!(verdict(&f).category, Category::Notifications);
    }

    #[test]
    fn a_conversation_takes_its_strongest_message_and_rules_beat_heuristics() {
        let news = v(Category::Newsletters, Why::Unsubscribe);
        let mine = v(Category::Important, Why::FromYou);
        let cc = v(Category::Other, Why::Rest);
        let bot = v(Category::Notifications, Why::NoReply);
        // You replied to a newsletter: Important.
        assert_eq!(conversation(&[news.clone(), mine.clone()], None).unwrap().category, Category::Important);
        // A person over a machine.
        assert_eq!(conversation(&[bot.clone(), cc.clone()], None).unwrap(), cc);
        assert_eq!(conversation(&[news.clone(), bot.clone()], None).unwrap(), bot);
        // A rule's verdict beats any heuristic's, even a stronger one.
        let ruled = v(Category::Notifications, Why::Rule(0, Rule::sender("notifications@github.com", Category::Notifications)));
        assert_eq!(conversation(&[mine.clone(), ruled.clone()], None).unwrap(), ruled);
        // A correction beats both.
        assert_eq!(conversation(&[mine, ruled], Some(Category::Newsletters)).unwrap(), v(Category::Newsletters, Why::Moved));
        assert_eq!(conversation(&[], None), None);
    }

    #[test]
    fn rules_match_senders_domains_patterns_and_lists_and_the_first_wins() {
        let r = |from: &str, list: &str, c| Rule { from: from.into(), list: list.into(), category: c };
        assert!(r("news@shop.example", "", Category::Newsletters).matches("News@Shop.Example", ""));
        assert!(!r("news@shop.example", "", Category::Newsletters).matches("other@shop.example", ""));
        for domain in ["shop.example", "@shop.example"] {
            let rule = r(domain, "", Category::Newsletters);
            assert!(rule.matches("a@shop.example", "") && rule.matches("a@mail.shop.example", ""), "{domain}");
            assert!(!rule.matches("a@badshop.example", "") && !rule.matches("shop.example@x.example", ""), "{domain}");
        }
        let glob = r("*@github.com", "", Category::Notifications);
        assert!(glob.matches("notifications@github.com", "") && !glob.matches("a@github.community", "") && !glob.matches("a@x.github.com", ""));
        assert!(r("noreply*", "", Category::Notifications).matches("noreply-abc@x.example", ""));
        assert!(r("*news*@*", "", Category::Newsletters).matches("weekly-news@x.example", ""));
        // A list id: exactly, a parent of it, or a pattern; never mail without one.
        let list = r("", "rust-lang.org", Category::Newsletters);
        assert!(list.matches("a@b.example", "users.rust-lang.org") && list.matches("a@b.example", "rust-lang.org"));
        assert!(!list.matches("a@b.example", "notrust-lang.org") && !list.matches("a@b.example", ""));
        // Both given: both must match.
        let both = r("*@github.com", "*.anomalyco.github.com", Category::Important);
        assert!(both.matches("notifications@github.com", "envoy.anomalyco.github.com") && !both.matches("notifications@github.com", "x.other.github.com"));
        // The first rule that matches decides, and rules beat the heuristics.
        let c = Classifier::new(vec![r("ci@github.com", "", Category::Important), glob.clone()], ["me@example.com".to_owned()]);
        let f = |from| Facts { from, signals: signals::KNOWN | signals::LIST_UNSUBSCRIBE, ..Facts::default() };
        assert_eq!(c.verdict(&f("ci@github.com")).category, Category::Important);
        assert!(matches!(c.verdict(&f("notifications@github.com")).why, Why::Rule(1, _)));
        assert_eq!(c.verdict(&f("hello@news.example")).why, Why::Unsubscribe);
    }

    #[test]
    fn rules_read_from_settings_and_text_and_say_whats_wrong() {
        let json = serde_json::json!([
            { "from": " *@GitHub.com ", "category": "notifications" },
            { "list": "rust-lang.org", "category": "newsletters" },
            { "from": "boss@work.example", "list": "", "category": "important" },
            { "from": "x", "category": "important" },
            { "category": "other" },
            { "from": "a@b.example", "category": "spam" },
            "from:a@b.example other"
        ]);
        let (rules, problems) = parse_rules(&json);
        assert_eq!(rules.iter().map(Rule::text).collect::<Vec<_>>(), ["from:*@github.com notifications", "list:rust-lang.org newsletters", "from:boss@work.example important"]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].starts_with("rule 4") && problems[1].starts_with("rule 5"));
        for r in &rules {
            assert_eq!(&Rule::parse_text(&r.text()).unwrap(), r, "round trip");
        }
        assert_eq!(Rule::parse_text("Newsletter FROM:News@Shop.example").unwrap(), Rule::sender("news@shop.example", Category::Newsletters));
        assert!(Rule::parse_text("from:a@b.example").is_err(), "no category");
        assert!(Rule::parse_text("from:a@b.example sometimes important").is_err());
        assert!(Rule::parse_text("important").is_err(), "matches nothing");
        assert!(Rule::parse_text("from:* important").is_err(), "matches everything");
        // Serialized as settings.json holds it: no empty fields.
        assert_eq!(rules[0].to_json(), serde_json::json!({ "from": "*@github.com", "category": "notifications" }));
    }

    #[test]
    fn only_corrections_for_single_senders_spare_a_full_pass() {
        let c = Classifier::new(vec![Rule::sender("a@x.example", Category::Newsletters)], ["me@example.com".to_owned()]);
        let mut next = c.clone();
        next.rules.insert(0, Rule::sender("b@y.example", Category::Notifications));
        assert_eq!(c.senders_changed(&next), Some(vec!["b@y.example".to_owned()]));
        assert_eq!(next.senders_changed(&c), Some(vec!["b@y.example".to_owned()]), "taken back");
        let mut moved = next.clone();
        moved.rules.reverse();
        assert_eq!(next.senders_changed(&moved), Some(vec!["a@x.example".to_owned(), "b@y.example".to_owned()]));
        let mut domain = c.clone();
        domain.rules.push(Rule { from: "*@z.example".into(), list: String::new(), category: Category::Other });
        assert_eq!(c.senders_changed(&domain), None);
        let other_me = Classifier::new(c.rules.clone(), ["you@example.com".to_owned()]);
        assert_eq!(c.senders_changed(&other_me), None);
        assert_eq!(c.senders_changed(&c), Some(vec![]));
        // The fingerprint reads back.
        assert_eq!(Classifier::from_fingerprint(&next.fingerprint()), Some(next.clone()));
        assert_eq!(Classifier::from_fingerprint(""), None);
    }

    #[test]
    fn categories_read_and_write_as_named() {
        for c in Category::ALL {
            assert_eq!(Category::from_code(c.code()), c);
            assert_eq!(Category::parse(c.as_str()), Some(c));
            assert_eq!(Category::parse(c.label()), Some(c));
        }
        assert_eq!(Category::parse("Newsletter"), Some(Category::Newsletters));
        assert_eq!(Category::parse("notification"), Some(Category::Notifications));
        assert_eq!(Category::parse("spam"), None);
        assert_eq!(Category::from_code(0), Category::Other, "the column's default");
    }
}
