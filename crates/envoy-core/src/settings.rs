//! Settings, VS Code style.
//!
//! Every setting is declared once in [`SCHEMA`] with its key, title, description, category,
//! type and default. `settings.json` holds only the user's overrides as flat dotted keys
//! (`"layout.readingPane": "bottom"`), so it stays short, diffs well and can be backed up or
//! copied to another machine. The Settings screen is generated from the schema, and editing
//! there rewrites the file; editing the file by hand is picked up live.
//!
//! The file may contain `//` and `/* */` comments and trailing commas when edited by hand.
//! Saving from the UI rewrites it as plain JSON.

use std::path::Path;
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    Number { min: f64, max: f64, step: f64 },
    /// `(value, label)` pairs.
    Enum(&'static [(&'static str, &'static str)]),
    Text { multiline: bool },
    /// One or more chords separated by commas: `"e"`, `"mod+enter"`, `"delete, #"`.
    Keybinding,
    /// A list of strings (a JSON array), trimmed, lowercased and without repeats.
    List,
    /// The split inbox's rules: a JSON array of `{ "from": …, "list": …, "category": … }`
    /// ([`crate::category::Rule`]), kept as written; entries that can't be read do nothing and
    /// are listed among the problems.
    Rules,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Num(f64),
    Str(&'static str),
    List(&'static [&'static str]),
}

impl DefaultValue {
    pub fn to_value(self) -> Value {
        match self {
            DefaultValue::Bool(b) => Value::Bool(b),
            DefaultValue::Num(n) => serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number),
            DefaultValue::Str(s) => Value::String(s.to_owned()),
            DefaultValue::List(l) => Value::Array(l.iter().map(|s| Value::String((*s).to_owned())).collect()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SettingDef {
    pub key: &'static str,
    pub category: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub kind: Kind,
    pub default: DefaultValue,
}

const fn def(
    key: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    kind: Kind,
    default: DefaultValue,
) -> SettingDef {
    SettingDef { key, category, title, description, kind, default }
}

use self::DefaultValue::{Bool as B, List as L, Num as N, Str as S};

pub const CATEGORIES: &[&str] = &["Appearance", "Inbox", "Reading", "Compose", "Sync", "Behavior", "Keyboard"];

pub const ACCENTS: &[(&str, &str)] = &[
    ("blue", "Blue"),
    ("violet", "Violet"),
    ("teal", "Teal"),
    ("green", "Green"),
    ("orange", "Orange"),
    ("pink", "Pink"),
    ("graphite", "Graphite"),
];

pub const SCHEMA: &[SettingDef] = &[
    // Appearance
    def("appearance.theme", "Appearance", "Theme", "Light, dark, or follow the system appearance.",
        Kind::Enum(&[("system", "System"), ("light", "Light"), ("dark", "Dark")]), S("system")),
    def("appearance.accent", "Appearance", "Accent color", "Color used for the compose button, selection and unread markers.",
        Kind::Enum(ACCENTS), S("blue")),
    def("appearance.density", "Appearance", "Density", "How much room each message row gets in the list.",
        Kind::Enum(&[("comfortable", "Comfortable"), ("compact", "Compact")]), S("comfortable")),
    def("appearance.windowFrame", "Appearance", "Framed content", "Draw the message area as an inset card beside the sidebar.",
        Kind::Bool, B(true)),
    // Inbox
    def("inbox.readingPane", "Inbox", "Reading pane", "Where an opened message shows: beside the list, below it, or full width replacing it.",
        Kind::Enum(&[("right", "Right of the list"), ("bottom", "Below the list"), ("off", "Off (full width)")]), S("right")),
    def("inbox.listWidth", "Inbox", "List width", "Width of the message list, in points, when the reading pane is on the right.",
        Kind::Number { min: 280.0, max: 900.0, step: 10.0 }, N(420.0)),
    def("inbox.listHeight", "Inbox", "List height", "Height of the message list, in points, when the reading pane is below it.",
        Kind::Number { min: 160.0, max: 900.0, step: 10.0 }, N(320.0)),
    def("inbox.showPreview", "Inbox", "Show preview text", "Show the first line of each message under its subject.",
        Kind::Bool, B(true)),
    def("inbox.showAvatars", "Inbox", "Show avatars", "Show a colored initial for each sender.",
        Kind::Bool, B(true)),
    def("inbox.conversationView", "Inbox", "Conversation view", "Group replies into one row per conversation, and show the whole conversation when opened.",
        Kind::Bool, B(true)),
    def("inbox.dateFormat", "Inbox", "Date format", "How dates show in the message list.",
        Kind::Enum(&[("relative", "Smart (10:42, Tue, Mar 4)"), ("short", "Short (2026-03-04)"), ("long", "Long (Mar 4, 2026, 10:42)")]), S("relative")),
    def("inbox.unifiedInbox", "Inbox", "Unified inbox", "With more than one account, show an inbox that combines all of them.",
        Kind::Bool, B(true)),
    def("inbox.split", "Inbox", "Split inbox",
        "Sort the inbox into Important, Other, Newsletters and Notifications, each with its unread count. Decided on this computer from each message's headers and your rules; nothing changes on the server. Search looks in all four unless asked (category:newsletters).",
        Kind::Bool, B(true)),
    def("inbox.rules", "Inbox", "Split inbox rules",
        "Where mail goes, before Envoy's own judgment. One rule per line: from: an address, a domain or a pattern such as *@github.com, and/or list: a mailing list's id, then a category (important, other, newsletters or notifications). The first rule that matches wins. Moving a conversation to a category adds a rule for its sender.",
        Kind::Rules, L(&[])),
    // Reading
    def("reading.markAsRead", "Reading", "Mark as read", "When an opened message is marked read.",
        Kind::Enum(&[("immediately", "Immediately"), ("delay", "After a delay"), ("never", "Only when I mark it")]), S("immediately")),
    def("reading.markAsReadDelay", "Reading", "Mark as read delay", "Seconds a message stays open before it is marked read (with 'After a delay').",
        Kind::Number { min: 0.5, max: 30.0, step: 0.5 }, N(2.0)),
    def("reading.loadImages", "Reading", "Load remote images",
        "Pictures HTML mail loads from the internet, which can tell the sender you opened it. Ask: hidden, with a bar to load them once or always from that sender. Pictures embedded in the message always show; tracking pixels never load.",
        Kind::Enum(&[("ask", "Ask (load from senders I allow)"), ("always", "Always"), ("never", "Never")]), S("ask")),
    def("reading.imagesFrom", "Reading", "Load images from", "Senders whose remote images load without asking. One per line: an address, or a domain such as example.com or @example.com (its subdomains too).",
        Kind::List, L(&[])),
    def("reading.htmlColors", "Reading", "HTML mail colors", "Adapt colors to the current theme, or show mail exactly as designed on a light page.",
        Kind::Enum(&[("adapt", "Adapt to theme"), ("original", "Original (light page)")]), S("adapt")),
    def("reading.preferPlainText", "Reading", "Prefer plain text", "Show the plain-text version of a message when it has one, instead of its formatted version.",
        Kind::Bool, B(false)),
    def("reading.maxWidth", "Reading", "Text column width", "Widest a message body grows, in points. Long lines wrap at this width.",
        Kind::Number { min: 400.0, max: 1600.0, step: 20.0 }, N(760.0)),
    def("reading.showQuoted", "Reading", "Show quoted text", "Show earlier messages quoted at the bottom of replies (lines starting with >).",
        Kind::Bool, B(false)),
    // Compose
    def("compose.signature", "Compose", "Signature", "Added to the end of new messages and replies. Leave empty for none.",
        Kind::Text { multiline: true }, S("")),
    def("compose.undoSendSeconds", "Compose", "Undo send", "Seconds a sent message waits in the outbox, so you can take it back. 0 sends at once.",
        Kind::Number { min: 0.0, max: 30.0, step: 1.0 }, N(5.0)),
    def("compose.defaultReplyAll", "Compose", "Reply all by default", "Make the reply shortcut reply to everyone on the message.",
        Kind::Bool, B(false)),
    def("compose.quoteOriginal", "Compose", "Quote original", "Include the message you are replying to, quoted, below your reply.",
        Kind::Bool, B(true)),
    // Sync
    def("sync.intervalSeconds", "Sync", "Check interval", "Seconds between checks for new mail. With push (IDLE) on, new inbox mail arrives within seconds anyway.",
        Kind::Number { min: 15.0, max: 3600.0, step: 15.0 }, N(120.0)),
    def("sync.useIdle", "Sync", "Push (IMAP IDLE)", "Keep a connection open so the server tells Envoy about new inbox mail right away.",
        Kind::Bool, B(true)),
    def("sync.initialMessages", "Sync", "Messages per folder", "How many of the newest messages in each folder are kept on this computer.",
        Kind::Number { min: 50.0, max: 50000.0, step: 50.0 }, N(2000.0)),
    def("sync.prefetchBodies", "Sync", "Download ahead", "How many of the newest messages per folder are downloaded in full, so they open instantly and work offline.",
        Kind::Number { min: 0.0, max: 5000.0, step: 50.0 }, N(300.0)),
    def("sync.maxPrefetchSizeKb", "Sync", "Download-ahead size limit", "Messages larger than this many kilobytes download only when opened.",
        Kind::Number { min: 32.0, max: 51200.0, step: 32.0 }, N(2048.0)),
    // Behavior
    def("behavior.confirmDelete", "Behavior", "Confirm delete", "Ask before moving a message to the trash.",
        Kind::Bool, B(false)),
    def("behavior.afterRemove", "Behavior", "After archive or delete", "What opens after you archive or delete the open message.",
        Kind::Enum(&[("next", "The next (older) message"), ("previous", "The previous (newer) message"), ("list", "Back to the list")]), S("next")),
    def("behavior.undoSeconds", "Behavior", "Undo window", "Seconds an archive, delete, spam or move waits before it reaches the server, so you can take it back with Undo. 0 applies it at once, with no undo.",
        Kind::Number { min: 0.0, max: 30.0, step: 1.0 }, N(5.0)),
    def("behavior.copyLinks", "Behavior", "Copy links instead of opening", "Clicking a link copies it rather than opening the browser.",
        Kind::Bool, B(false)),
    // Keyboard
    def("keys.compose", "Keyboard", "Compose", "Start a new message.", Kind::Keybinding, S("c, mod+n")),
    def("keys.reply", "Keyboard", "Reply", "Reply to the open message.", Kind::Keybinding, S("r, mod+r")),
    def("keys.replyAll", "Keyboard", "Reply all", "Reply to everyone on the open message.", Kind::Keybinding, S("a, mod+shift+r")),
    def("keys.forward", "Keyboard", "Forward", "Forward the open message.", Kind::Keybinding, S("f, mod+shift+f")),
    def("keys.archive", "Keyboard", "Archive", "Archive the open message.", Kind::Keybinding, S("e, y")),
    def("keys.delete", "Keyboard", "Delete", "Move the open message to the trash.", Kind::Keybinding, S("#, delete, backspace")),
    def("keys.undo", "Keyboard", "Undo", "Take back the last archive, delete, move or send while its notice is showing.", Kind::Keybinding, S("z")),
    def("keys.star", "Keyboard", "Star", "Star or unstar the open message.", Kind::Keybinding, S("s")),
    def("keys.markUnread", "Keyboard", "Toggle read", "Mark the open message read or unread.", Kind::Keybinding, S("u, mod+shift+u")),
    def("keys.next", "Keyboard", "Next message", "Open the next (older) message.", Kind::Keybinding, S("j, down")),
    def("keys.previous", "Keyboard", "Previous message", "Open the previous (newer) message.", Kind::Keybinding, S("k, up")),
    def("keys.search", "Keyboard", "Search", "Focus the search field.", Kind::Keybinding, S("/, mod+f")),
    def("keys.refresh", "Keyboard", "Check mail", "Check for new mail now.", Kind::Keybinding, S("mod+shift+n, f5")),
    def("keys.settings", "Keyboard", "Settings", "Open settings.", Kind::Keybinding, S("mod+comma")),
    def("keys.send", "Keyboard", "Send", "Send the message being written.", Kind::Keybinding, S("mod+enter")),
    def("keys.attach", "Keyboard", "Attach files", "Attach files to the message being written. You can also drop files on it.", Kind::Keybinding, S("mod+shift+a")),
    def("keys.close", "Keyboard", "Close", "Close the open message, the composer or settings.", Kind::Keybinding, S("escape")),
    def("keys.goInbox", "Keyboard", "Go to inbox", "Show the inbox.", Kind::Keybinding, S("mod+1")),
    def("keys.goStarred", "Keyboard", "Go to starred", "Show starred messages.", Kind::Keybinding, S("mod+2")),
    def("keys.goSent", "Keyboard", "Go to sent", "Show sent messages.", Kind::Keybinding, S("mod+3")),
    def("keys.select", "Keyboard", "Select", "Select the message, or unselect it. Archive, delete, spam, read, star and move then act on every selected message.", Kind::Keybinding, S("x")),
    def("keys.selectNext", "Keyboard", "Extend selection down", "Select the next (older) message too.", Kind::Keybinding, S("shift+j, shift+down")),
    def("keys.selectPrevious", "Keyboard", "Extend selection up", "Select the previous (newer) message too.", Kind::Keybinding, S("shift+k, shift+up")),
    def("keys.selectAll", "Keyboard", "Select all", "Select every message in the list.", Kind::Keybinding, S("mod+a")),
    def("keys.spam", "Keyboard", "Report spam", "Move the open or selected messages to Spam (or back to the inbox, from Spam).", Kind::Keybinding, S("!")),
    def("keys.moveTo", "Keyboard", "Move to…", "Move the open or selected messages to a folder you pick.", Kind::Keybinding, S("v")),
    def("keys.label", "Keyboard", "Label…", "Add a label to the open or selected messages (Gmail).", Kind::Keybinding, S("l")),
    def("keys.showImportant", "Keyboard", "Show Important", "Show the split inbox's Important mail.", Kind::Keybinding, S("1")),
    def("keys.showOther", "Keyboard", "Show Other", "Show the split inbox's Other mail.", Kind::Keybinding, S("2")),
    def("keys.showNewsletters", "Keyboard", "Show Newsletters", "Show the split inbox's newsletters.", Kind::Keybinding, S("3")),
    def("keys.showNotifications", "Keyboard", "Show Notifications", "Show the split inbox's notifications.", Kind::Keybinding, S("4")),
    def("keys.moveImportant", "Keyboard", "Move to Important", "Move the open or selected conversations to Important, and future mail from their senders.", Kind::Keybinding, S("alt+1")),
    def("keys.moveOther", "Keyboard", "Move to Other", "Move the open or selected conversations to Other, and future mail from their senders.", Kind::Keybinding, S("alt+2")),
    def("keys.moveNewsletters", "Keyboard", "Move to Newsletters", "Move the open or selected conversations to Newsletters, and future mail from their senders.", Kind::Keybinding, S("alt+3")),
    def("keys.moveNotifications", "Keyboard", "Move to Notifications", "Move the open or selected conversations to Notifications, and future mail from their senders.", Kind::Keybinding, S("alt+4")),
    def("keys.snooze", "Keyboard", "Snooze…", "Put the open or selected messages away until a time you pick; they come back to the top of the inbox then. In Snoozed, pick a new time.", Kind::Keybinding, S("b")),
];

pub fn schema(key: &str) -> Option<&'static SettingDef> {
    SCHEMA.iter().find(|d| d.key == key)
}

/// The user's settings: overrides on top of the schema's defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    user: Map<String, Value>,
    /// Problems found while reading the file, shown in the Settings screen.
    pub problems: Vec<String>,
    mtime: Option<SystemTime>,
}

impl std::default::Default for Settings {
    fn default() -> Self {
        Settings { user: Map::new(), problems: Vec::new(), mtime: None }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Settings {
    pub fn load() -> Settings {
        Self::load_from(&paths::settings_file())
    }

    pub fn load_from(path: &Path) -> Settings {
        let mut s = Settings { mtime: mtime(path), ..Settings::default() };
        let Ok(raw) = std::fs::read_to_string(path) else { return s };
        let text = strip_jsonc(&raw);
        if text.trim().is_empty() {
            return s;
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => {
                for (k, v) in map {
                    match schema(&k) {
                        None => s.problems.push(format!("Unknown setting \"{k}\"")),
                        Some(d) => match validate(d, &v) {
                            Ok(v) => {
                                if d.kind == Kind::Rules {
                                    s.problems.extend(crate::category::parse_rules(&v).1.into_iter().map(|p| format!("\"{k}\": {p}")));
                                }
                                s.user.insert(k, v);
                            }
                            Err(e) => s.problems.push(format!("\"{k}\": {e}")),
                        },
                    }
                }
            }
            Ok(_) => s.problems.push("settings.json must contain a JSON object".into()),
            Err(e) => s.problems.push(format!("settings.json: {e}")),
        }
        s
    }

    /// True when the file changed on disk since it was loaded.
    pub fn changed_on_disk(&self) -> bool {
        mtime(&paths::settings_file()) != self.mtime
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = paths::settings_file();
        let mut sorted: Vec<_> = self.user.iter().collect();
        sorted.sort_by_key(|(k, _)| SCHEMA.iter().position(|d| d.key == k.as_str()).unwrap_or(usize::MAX));
        let map: Map<String, Value> = sorted.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let text = serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".into());
        paths::write_atomic(&path, format!("{text}\n").as_bytes())?;
        self.mtime = mtime(&path);
        Ok(())
    }

    pub fn value(&self, key: &str) -> Value {
        if let Some(v) = self.user.get(key) {
            return v.clone();
        }
        schema(key).map_or(Value::Null, |d| d.default.to_value())
    }

    pub fn is_default(&self, key: &str) -> bool {
        !self.user.contains_key(key)
    }

    /// Sets a value (validated against the schema). Setting the default removes the override.
    pub fn set(&mut self, key: &str, v: Value) -> Result<(), String> {
        let d = schema(key).ok_or_else(|| format!("unknown setting {key}"))?;
        let v = validate(d, &v)?;
        if v == d.default.to_value() {
            self.user.remove(key);
        } else {
            self.user.insert(key.to_owned(), v);
        }
        Ok(())
    }

    pub fn reset(&mut self, key: &str) {
        self.user.remove(key);
    }

    pub fn bool(&self, key: &str) -> bool {
        self.value(key).as_bool().unwrap_or(false)
    }

    pub fn f64(&self, key: &str) -> f64 {
        self.value(key).as_f64().unwrap_or(0.0)
    }

    pub fn f32(&self, key: &str) -> f32 {
        self.f64(key) as f32
    }

    pub fn u32(&self, key: &str) -> u32 {
        self.f64(key).max(0.0) as u32
    }

    pub fn str(&self, key: &str) -> String {
        match self.value(key) {
            Value::String(s) => s,
            _ => String::new(),
        }
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        match self.value(key) {
            Value::Array(a) => a.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether remote images in mail from `sender` load without asking: `reading.loadImages`
    /// is "always", or "ask" with the sender (or its domain) in `reading.imagesFrom`.
    pub fn remote_images_from(&self, sender: &str) -> bool {
        match self.str("reading.loadImages").as_str() {
            "always" => true,
            "ask" => {
                let sender = sender.trim().to_lowercase();
                let domain = sender.rsplit_once('@').map_or("", |(_, d)| d);
                self.list("reading.imagesFrom").iter().filter_map(|e| image_sender(e)).any(|e| match e {
                    ImageSender::Address(a) => a == sender,
                    ImageSender::Domain(d) => !domain.is_empty() && (domain == d || domain.strip_suffix(d).is_some_and(|sub| sub.ends_with('.'))),
                })
            }
            _ => false,
        }
    }

    /// The user's overrides, for "show only modified".
    pub fn overrides(&self) -> impl Iterator<Item = &str> {
        self.user.keys().map(String::as_str)
    }
}

/// What a `reading.imagesFrom` entry allows.
#[derive(Clone, Debug, PartialEq)]
pub enum ImageSender<'a> {
    /// This address only.
    Address(&'a str),
    /// This domain and its subdomains.
    Domain(&'a str),
}

/// Reads a `reading.imagesFrom` entry (already trimmed and lowercased): exactly `user@domain`,
/// `domain`, or `@domain`. Anything else allows nothing: `@@northwind.dev`,
/// `@maya@northwind.dev`, `maya@`, and a bare top-level domain (`dev`), which would cover every
/// sender under it.
pub fn image_sender(entry: &str) -> Option<ImageSender<'_>> {
    if let Some(domain) = entry.strip_prefix('@') {
        return is_domain(domain).then_some(ImageSender::Domain(domain));
    }
    match entry.split_once('@') {
        None => is_domain(entry).then_some(ImageSender::Domain(entry)),
        Some((user, domain)) => {
            let user_ok = !user.is_empty() && user.chars().all(|c| c.is_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c));
            (user_ok && is_domain(domain)).then_some(ImageSender::Address(entry))
        }
    }
}

/// Two or more dot-separated labels of letters, digits and inner hyphens.
fn is_domain(d: &str) -> bool {
    let labels: Vec<&str> = d.split('.').collect();
    labels.len() >= 2 && d.len() <= 253 && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

/// Entries of a list setting that are ignored because they can't be read (shown under the
/// field in Settings).
pub fn list_problems<'a>(key: &str, items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    match key {
        "reading.imagesFrom" => items.into_iter().map(|i| i.trim().to_lowercase()).filter(|i| !i.is_empty() && image_sender(i).is_none()).collect(),
        _ => Vec::new(),
    }
}

/// Removes `//` and `/* */` comments and trailing commas, leaving strings alone.
pub fn strip_jsonc(src: &str) -> String {
    let mut no_comments = String::new();
    let _ = std::io::Read::read_to_string(&mut json_comments::StripComments::new(src.as_bytes()), &mut no_comments);
    let chars: Vec<char> = no_comments.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let (mut in_str, mut escaped) = (false, false);
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some('}') | Some(']') | None) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn validate(d: &SettingDef, v: &Value) -> Result<Value, String> {
    match d.kind {
        Kind::Bool => v.as_bool().map(Value::Bool).ok_or_else(|| "expected true or false".into()),
        Kind::Number { min, max, .. } => {
            let n = v.as_f64().ok_or("expected a number")?;
            let n = n.clamp(min, max);
            serde_json::Number::from_f64(n).map(Value::Number).ok_or_else(|| "not a finite number".into())
        }
        Kind::Enum(options) => {
            let s = v.as_str().ok_or("expected a string")?;
            if options.iter().any(|(o, _)| *o == s) {
                Ok(Value::String(s.to_owned()))
            } else {
                let names: Vec<_> = options.iter().map(|(o, _)| format!("\"{o}\"")).collect();
                Err(format!("expected one of {}", names.join(", ")))
            }
        }
        Kind::Text { .. } | Kind::Keybinding => v.as_str().map(|s| Value::String(s.to_owned())).ok_or_else(|| "expected a string".into()),
        Kind::List => {
            let items = v.as_array().ok_or("expected a list of strings")?;
            let mut out: Vec<Value> = Vec::new();
            for item in items {
                let s = item.as_str().ok_or("expected a list of strings")?.trim().to_lowercase();
                if !s.is_empty() && !out.iter().any(|o| o.as_str() == Some(s.as_str())) {
                    out.push(Value::String(s));
                }
            }
            Ok(Value::Array(out))
        }
        // Kept as written, so a hand edit with a typo isn't lost the next time the file is saved.
        Kind::Rules => v.as_array().map(|_| v.clone()).ok_or_else(|| "expected a list of rules".into()),
    }
}

/// The split inbox's rules as Settings shows them: one per line ([`crate::category::Rule::text`]),
/// and entries that can't be read, as they are.
pub fn rules_text(v: &Value) -> String {
    let items = v.as_array().cloned().unwrap_or_default();
    items
        .iter()
        .map(|item| match crate::category::parse_rules(&Value::Array(vec![item.clone()])).0.first() {
            Some(rule) => rule.text(),
            None => item.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads rules typed one per line ([`rules_text`]'s form): the list to save, and the lines that
/// aren't rules, with why.
pub fn rules_from_text(text: &str) -> (Value, Vec<String>) {
    let mut rules = Vec::new();
    let mut problems = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match crate::category::Rule::parse_text(line) {
            Ok(r) => rules.push(r.to_json()),
            Err(e) => problems.push(format!("{line}: {e}")),
        }
    }
    (Value::Array(rules), problems)
}

/// A commented reference of every setting and its default, written next to `settings.json`
/// as `settings.defaults.jsonc` so hand editors can see everything available.
pub fn defaults_reference() -> String {
    let mut out = String::from("// Every Envoy setting with its default. Copy lines into settings.json to change them.\n{\n");
    let mut last_cat = "";
    for d in SCHEMA {
        if d.category != last_cat {
            out.push_str(&format!("\n  // ── {} ──\n", d.category));
            last_cat = d.category;
        }
        out.push_str(&format!("  // {}: {}\n", d.title, d.description));
        if let Kind::Enum(opts) = d.kind {
            let names: Vec<_> = opts.iter().map(|(o, _)| *o).collect();
            out.push_str(&format!("  // One of: {}\n", names.join(", ")));
        }
        let v = serde_json::to_string(&d.default.to_value()).unwrap_or_default();
        out.push_str(&format!("  \"{}\": {},\n", d.key, v));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        for d in SCHEMA {
            assert!(validate(d, &d.default.to_value()).is_ok(), "{}", d.key);
        }
    }

    #[test]
    fn comments_and_trailing_commas_parse() {
        let dir = std::env::temp_dir().join(format!("envoy-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n // hi\n \"inbox.readingPane\": \"bottom\", /* x */\n \"sync.intervalSeconds\": 5,\n \"nope\": 1,\n}").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.str("inbox.readingPane"), "bottom");
        assert_eq!(s.f64("sync.intervalSeconds"), 15.0, "clamped to min");
        assert_eq!(s.problems.len(), 1);
        assert!(s.bool("inbox.showPreview"));
    }

    #[test]
    fn remote_images_are_blocked_by_default_and_allowed_per_sender() {
        // The North Star: remote content blocked by default; remote images per-sender opt-in.
        let mut s = Settings::default();
        assert_eq!(s.str("reading.loadImages"), "ask");
        assert!(!s.remote_images_from("news@shop.example"));
        s.set("reading.imagesFrom", serde_json::json!([" News@Shop.Example ", "example.org", "news@shop.example", "", "@corp.example"])).unwrap();
        assert_eq!(s.list("reading.imagesFrom"), ["news@shop.example", "example.org", "@corp.example"], "trimmed, lowercased, once each");
        assert!(s.remote_images_from("NEWS@shop.example"));
        assert!(!s.remote_images_from("other@shop.example"), "an address entry is that address only");
        assert!(s.remote_images_from("a@example.org") && s.remote_images_from("a@mail.example.org"), "a domain and its subdomains");
        assert!(!s.remote_images_from("a@badexample.org") && !s.remote_images_from("example.org"), "not lookalikes, not a bare domain");
        assert!(s.remote_images_from("hr@corp.example"));
        s.set("reading.loadImages", serde_json::json!("never")).unwrap();
        assert!(!s.remote_images_from("news@shop.example"), "never means never");
        s.set("reading.loadImages", serde_json::json!("always")).unwrap();
        assert!(s.remote_images_from("anyone@anywhere.example"));
        assert!(s.set("reading.imagesFrom", serde_json::json!("a@b.c")).is_err(), "a list, not a string");
    }

    #[test]
    fn malformed_image_senders_allow_nothing() {
        // `trim_start_matches('@')` once turned these into grants.
        let mut s = Settings::default();
        let bad = ["@@northwind.dev", "@maya@northwind.dev", "maya@@northwind.dev", "maya@", "@", "dev", "@dev", "northwind..dev", ".northwind.dev", "-x.dev", "maya@north wind.dev", "<maya@northwind.dev>", "*.northwind.dev"];
        s.set("reading.imagesFrom", serde_json::json!(bad)).unwrap();
        for sender in ["maya@northwind.dev", "a@mail.northwind.dev", "anyone@x.dev"] {
            assert!(!s.remote_images_from(sender), "{sender} allowed by a malformed entry");
        }
        assert_eq!(list_problems("reading.imagesFrom", bad), bad.map(str::to_owned), "each one shown as ignored");
        let good = ["maya@northwind.dev", "northwind.dev", "@northwind.dev", "Ann.Lee+news@Mail.Example.co.uk", "bücher.example"];
        assert!(list_problems("reading.imagesFrom", good).is_empty());
        s.set("reading.imagesFrom", serde_json::json!(["@northwind.dev"])).unwrap();
        assert!(s.remote_images_from("maya@northwind.dev") && s.remote_images_from("a@mail.northwind.dev"));
        s.set("reading.imagesFrom", serde_json::json!(["maya@northwind.dev"])).unwrap();
        assert!(s.remote_images_from("Maya@Northwind.dev") && !s.remote_images_from("bo@northwind.dev"));
        assert!(list_problems("inbox.other", ["@@x"]).is_empty(), "other lists aren't read as senders");
    }

    #[test]
    fn rules_keep_what_was_written_and_say_whats_wrong() {
        let dir = std::env::temp_dir().join(format!("envoy-settings-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{ "inbox.rules": [ { "from": "*@GitHub.com", "category": "notifications" }, { "list": "rust-lang.org", "category": "newsletters" }, { "from": "x", "category": "important" } ] }"#,
        )
        .unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.value("inbox.rules").as_array().unwrap().len(), 3, "kept as written, the bad one too");
        assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
        assert!(s.problems[0].starts_with("\"inbox.rules\": rule 3"), "{:?}", s.problems);
        let text = rules_text(&s.value("inbox.rules"));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..2], ["from:*@github.com notifications", "list:rust-lang.org newsletters"]);
        assert!(lines[2].contains("\"from\":\"x\""), "shown as it is: {}", lines[2]);
        let (v, problems) = rules_from_text("from:*@github.com notifications\n\nnonsense\n  list:rust-lang.org newsletter ");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(v, serde_json::json!([{ "from": "*@github.com", "category": "notifications" }, { "list": "rust-lang.org", "category": "newsletters" }]));
        assert!(Settings::default().bool("inbox.split"), "on by default");
        assert!(Settings::default().set("inbox.rules", serde_json::json!("from:a@b.c other")).is_err(), "a list");
    }

    #[test]
    fn setting_default_removes_override() {
        let mut s = Settings::default();
        s.set("inbox.showPreview", Value::Bool(false)).unwrap();
        assert!(!s.is_default("inbox.showPreview"));
        s.set("inbox.showPreview", Value::Bool(true)).unwrap();
        assert!(s.is_default("inbox.showPreview"));
        assert!(s.set("inbox.readingPane", Value::String("left".into())).is_err());
    }
}
