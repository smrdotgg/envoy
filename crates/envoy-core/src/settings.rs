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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Num(f64),
    Str(&'static str),
}

impl DefaultValue {
    pub fn to_value(self) -> Value {
        match self {
            DefaultValue::Bool(b) => Value::Bool(b),
            DefaultValue::Num(n) => serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number),
            DefaultValue::Str(s) => Value::String(s.to_owned()),
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

use self::DefaultValue::{Bool as B, Num as N, Str as S};

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
    // Reading
    def("reading.markAsRead", "Reading", "Mark as read", "When an opened message is marked read.",
        Kind::Enum(&[("immediately", "Immediately"), ("delay", "After a delay"), ("never", "Only when I mark it")]), S("immediately")),
    def("reading.markAsReadDelay", "Reading", "Mark as read delay", "Seconds a message stays open before it is marked read (with 'After a delay').",
        Kind::Number { min: 0.5, max: 30.0, step: 0.5 }, N(2.0)),
    def("reading.loadImages", "Reading", "Show images", "Download pictures in HTML mail. Tracking pixels (tiny images) are never loaded. Off shows a placeholder instead.",
        Kind::Enum(&[("always", "Always"), ("never", "Never")]), S("always")),
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
    def("keys.close", "Keyboard", "Close", "Close the open message, the composer or settings.", Kind::Keybinding, S("escape")),
    def("keys.goInbox", "Keyboard", "Go to inbox", "Show the inbox.", Kind::Keybinding, S("mod+1")),
    def("keys.goStarred", "Keyboard", "Go to starred", "Show starred messages.", Kind::Keybinding, S("mod+2")),
    def("keys.goSent", "Keyboard", "Go to sent", "Show sent messages.", Kind::Keybinding, S("mod+3")),
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

    /// The user's overrides, for "show only modified".
    pub fn overrides(&self) -> impl Iterator<Item = &str> {
        self.user.keys().map(String::as_str)
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
    }
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
    fn setting_default_removes_override() {
        let mut s = Settings::default();
        s.set("inbox.showPreview", Value::Bool(false)).unwrap();
        assert!(!s.is_default("inbox.showPreview"));
        s.set("inbox.showPreview", Value::Bool(true)).unwrap();
        assert!(s.is_default("inbox.showPreview"));
        assert!(s.set("inbox.readingPane", Value::String("left".into())).is_err());
    }
}
