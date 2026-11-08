//! The window's state and everything that changes it.

use std::collections::{HashMap, HashSet};

use rune::{KeyEvent, Keybind, TextEdit, Tx};
use rune_kit::PaneState;
use serde_json::Value;

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::engine::{Command, Engine, Event, MoveTarget};
use envoy_core::model::{Draft, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};
use envoy_core::settings::Settings;

use crate::fmt;

pub enum ImageState {
    Loading,
    Ready(rune::Image),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Mail,
    Settings,
    AddAccount,
}

pub struct Compose {
    pub account: String,
    pub to: TextEdit,
    pub cc: TextEdit,
    pub bcc: TextEdit,
    pub subject: TextEdit,
    pub body: TextEdit,
    pub show_cc: bool,
    pub in_reply_to: String,
    pub references: String,
    pub reply_of: Option<MessageId>,
    pub title: String,
    pub minimized: bool,
    pub error: Option<String>,
    pub from_open: bool,
}

pub struct Toast {
    pub id: u64,
    pub text: String,
    pub undo: Option<Undo>,
    pub until: f64,
    pub error: bool,
    pub open_path: Option<std::path::PathBuf>,
}

/// What a toast's Undo takes back.
pub enum Undo {
    /// A message waiting in the outbox (the op id).
    Send(i64),
    /// An archive, delete or move that hasn't reached the server.
    Move(Box<MovedAway>),
}

/// Enough to put moved messages back on screen at once, before the engine confirms.
pub struct MovedAway {
    /// The key the engine holds the move's ops under.
    pub key: i64,
    /// The view and search they were removed from; they're put back only if those are still
    /// showing (otherwise the reload after undo decides what belongs in the list).
    pub view: View,
    pub query: String,
    pub rows: Vec<Summary>,
    /// The open message, if it was one of them.
    pub open: Option<MessageId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddMode {
    Google,
    AppPassword,
    Imap,
}

pub struct AddForm {
    pub mode: AddMode,
    pub email: TextEdit,
    pub name: TextEdit,
    pub password: TextEdit,
    pub imap_host: TextEdit,
    pub imap_port: TextEdit,
    pub smtp_host: TextEdit,
    pub smtp_port: TextEdit,
    pub client_id: TextEdit,
    pub client_secret: TextEdit,
    pub busy: bool,
    pub error: Option<String>,
    pub show_password: bool,
}

impl AddForm {
    pub fn new() -> AddForm {
        let (cid, csec) = envoy_core::oauth::default_client();
        AddForm {
            mode: if cid.is_empty() { AddMode::AppPassword } else { AddMode::Google },
            email: TextEdit::new(""),
            name: TextEdit::new(""),
            password: TextEdit::new(""),
            imap_host: TextEdit::new(""),
            imap_port: TextEdit::new("993"),
            smtp_host: TextEdit::new(""),
            smtp_port: TextEdit::new("465"),
            client_id: TextEdit::new(cid),
            client_secret: TextEdit::new(csec),
            busy: false,
            error: None,
            show_password: false,
        }
    }
}

pub struct SettingsUi {
    pub query: TextEdit,
    /// Index into `settings::CATEGORIES`, or `None` for every setting.
    pub category: Option<usize>,
    pub accounts: bool,
    pub modified_only: bool,
    pub open_select: Option<String>,
    /// Text fields for number, text and keybinding settings, by key.
    pub edits: HashMap<String, TextEdit>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    Compose,
    Reply,
    ReplyAll,
    Forward,
    Archive,
    Delete,
    Undo,
    Star,
    ToggleRead,
    Next,
    Previous,
    Search,
    Refresh,
    Settings,
    Send,
    Close,
    GoInbox,
    GoStarred,
    GoSent,
}

const ACTIONS: &[(Action, &str)] = &[
    (Action::Compose, "keys.compose"),
    (Action::Reply, "keys.reply"),
    (Action::ReplyAll, "keys.replyAll"),
    (Action::Forward, "keys.forward"),
    (Action::Archive, "keys.archive"),
    (Action::Delete, "keys.delete"),
    (Action::Undo, "keys.undo"),
    (Action::Star, "keys.star"),
    (Action::ToggleRead, "keys.markUnread"),
    (Action::Next, "keys.next"),
    (Action::Previous, "keys.previous"),
    (Action::Search, "keys.search"),
    (Action::Refresh, "keys.refresh"),
    (Action::Settings, "keys.settings"),
    (Action::Send, "keys.send"),
    (Action::Close, "keys.close"),
    (Action::GoInbox, "keys.goInbox"),
    (Action::GoStarred, "keys.goStarred"),
    (Action::GoSent, "keys.goSent"),
];

pub struct App {
    pub engine: Option<Engine>,
    pub settings: Settings,
    pub keys: Vec<(Action, Keybind)>,
    pub accounts: Vec<Account>,
    pub mailboxes: HashMap<String, Vec<Mailbox>>,
    pub sync: HashMap<String, SyncState>,
    pub pending_ops: u32,
    pub screen: Screen,
    pub view: View,
    pub list: Vec<Summary>,
    pub list_req: u64,
    pub list_loaded: bool,
    /// The opened row (its newest message in conversation view).
    pub open: Option<MessageId>,
    pub thread: Vec<ThreadMessage>,
    pub thread_req: u64,
    pub thread_for: Option<MessageId>,
    /// Collapsed / expanded messages in the open conversation.
    pub expanded: HashSet<MessageId>,
    pub show_quoted: HashSet<MessageId>,
    pub search: TextEdit,
    pub applied_query: String,
    pub compose: Option<Compose>,
    pub toasts: Vec<Toast>,
    pub toast_seq: u64,
    /// The last undo key handed to the engine (keys must be unique across runs).
    pub undo_key: i64,
    pub sidebar: PaneState,
    pub collapsed_sections: HashSet<String>,
    pub settings_ui: SettingsUi,
    pub add: AddForm,
    pub mark_read_at: Option<(MessageId, f64)>,
    pub clock: f64,
    pub list_drag_start: Option<f32>,
    pub list_width: f32,
    pub list_height: f32,
    pub demo: bool,
    pub settings_checked_at: f64,
    pub confirm_delete: Option<Vec<MessageId>>,
    pub images: HashMap<String, ImageState>,
    pub image_cache: Option<rune::ImageCache>,
}

pub fn parse_keys(s: &Settings) -> Vec<(Action, Keybind)> {
    ACTIONS.iter().map(|(a, key)| (*a, keybind(&s.str(key)))).collect()
}

/// Parses a user's keybinding, skipping chords that don't parse (they're flagged in Settings).
pub fn keybind(spec: &str) -> Keybind {
    let valid: Vec<&str> = spec.split(',').map(str::trim).filter(|p| !p.is_empty() && p.parse::<rune::Chord>().is_ok()).collect();
    valid.join(", ").parse().unwrap_or_else(|_| "none".parse().expect("\"none\" is an empty keybind"))
}

impl App {
    pub fn new(demo: bool) -> App {
        let settings = if demo { Settings::default() } else { Settings::load() };
        let list_width = settings.f32("inbox.listWidth");
        let list_height = settings.f32("inbox.listHeight");
        App {
            engine: None,
            keys: parse_keys(&settings),
            settings,
            accounts: Vec::new(),
            mailboxes: HashMap::new(),
            sync: HashMap::new(),
            pending_ops: 0,
            screen: Screen::Mail,
            view: View::Role { account: None, role: Role::Inbox },
            list: Vec::new(),
            list_req: 0,
            list_loaded: false,
            open: None,
            thread: Vec::new(),
            thread_req: 0,
            thread_for: None,
            expanded: HashSet::new(),
            show_quoted: HashSet::new(),
            search: TextEdit::new(""),
            applied_query: String::new(),
            compose: None,
            toasts: Vec::new(),
            toast_seq: 0,
            undo_key: 0,
            sidebar: PaneState::new(248.0).range(180.0, 420.0),
            collapsed_sections: HashSet::new(),
            settings_ui: SettingsUi {
                query: TextEdit::new(""),
                category: None,
                accounts: false,
                modified_only: false,
                open_select: None,
                edits: HashMap::new(),
            },
            add: AddForm::new(),
            mark_read_at: None,
            clock: 0.0,
            list_drag_start: None,
            list_width,
            list_height,
            demo,
            settings_checked_at: 0.0,
            confirm_delete: None,
            images: HashMap::new(),
            image_cache: None,
        }
    }

    pub fn send(&self, c: Command) {
        if let Some(e) = &self.engine {
            e.send(c);
        }
    }

    pub fn conversations(&self) -> bool {
        self.settings.bool("inbox.conversationView")
    }

    // ── engine events ──

    /// Drains engine events; true when anything visible changed.
    pub fn pump(&mut self, tx: &mut Tx) -> bool {
        let events = match &self.engine {
            Some(e) => e.drain(),
            None => return false,
        };
        let changed = !events.is_empty();
        let mut reload = false;
        for ev in events {
            reload |= self.apply(ev, tx);
        }
        if reload {
            self.reload();
        }
        changed
    }

    /// Applies one engine event. True when what's on screen should be re-read.
    fn apply(&mut self, ev: Event, tx: &mut Tx) -> bool {
        let mut reload = false;
        match ev {
            Event::Accounts(a) => {
                let had = !self.accounts.is_empty();
                self.accounts = a;
                if self.accounts.is_empty() && self.screen == Screen::Mail {
                    self.screen = Screen::AddAccount;
                } else if !had && !self.accounts.is_empty() && self.screen == Screen::AddAccount {
                    self.screen = Screen::Mail;
                }
                if let View::Mailbox { account, .. } = &self.view
                    && !self.accounts.iter().any(|a| &a.id == account)
                {
                    self.view = View::Role { account: None, role: Role::Inbox };
                }
                reload = true;
            }
            Event::Mailboxes { account, mailboxes } => {
                self.mailboxes.insert(account, mailboxes);
            }
            Event::List { req, items } => {
                if req == self.list_req {
                    self.list = items;
                    self.list_loaded = true;
                }
            }
            Event::Thread { req, id, messages } if req == self.thread_req && messages.is_empty() => {
                // The open message is gone (moved, deleted, or an undo that came too late):
                // close it rather than show an empty reader or mark it read later.
                if self.open == Some(id) || self.thread_for == Some(id) {
                    self.close_message();
                }
            }
            Event::Thread { req, id, messages } => {
                if req == self.thread_req {
                    let first = self.thread_for != Some(id) || self.thread.is_empty();
                    self.thread_for = Some(id);
                    if first {
                        self.expanded.clear();
                        // Unread messages and the newest start open, as Gmail does.
                        for (i, m) in messages.iter().enumerate() {
                            if !m.summary.seen() || i + 1 == messages.len() {
                                self.expanded.insert(m.summary.id);
                            }
                        }
                    }
                    self.thread = messages;
                    self.request_images();
                }
            }
            Event::Image { url, width, height, rgba } => {
                if let Some(cache) = &self.image_cache {
                    self.images.insert(url, ImageState::Ready(cache.rgba(width, height, rgba)));
                }
            }
            Event::ImageFailed { url } => {
                self.images.insert(url, ImageState::Failed);
            }
            Event::Changed { .. } => reload = true,
            Event::Sync { account, state } => {
                self.sync.insert(account, state);
            }
            Event::Toast(t) => self.toast(t, false),
            Event::Error(e) => self.toast(e, true),
            Event::Sent { op, undo_secs } => {
                self.toast_seq += 1;
                let id = self.toast_seq;
                let text = if undo_secs > 0 { "Sending…".to_owned() } else { "Sending".to_owned() };
                self.toasts.push(Toast {
                    id,
                    text,
                    undo: (undo_secs > 0).then_some(Undo::Send(op)),
                    until: self.clock + undo_secs.max(3) as f64,
                    error: false,
                    open_path: None,
                });
                tx.wake_after(std::time::Duration::from_secs_f64(undo_secs.max(3) as f64 + 0.05));
            }
            Event::AccountAdded(a) => {
                self.add = AddForm::new();
                self.screen = Screen::Mail;
                self.toast(format!("Added {}. Syncing…", a.email), false);
                self.view = View::Role { account: None, role: Role::Inbox };
                reload = true;
            }
            Event::AccountFailed(e) => {
                self.add.busy = false;
                self.add.error = Some(e);
            }
            Event::GoogleSignedIn { email, name, refresh_token } => {
                let account = self.google_account(&email, &name);
                self.add.error = None;
                self.send(Command::AddAccount { account, secret: refresh_token });
            }
            Event::GoogleFailed(e) => {
                self.add.busy = false;
                self.add.error = Some(e);
            }
            Event::Saved(path) => {
                self.toast_seq += 1;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.toasts.push(Toast { id: self.toast_seq, text: format!("Saved {name}"), undo: None, until: self.clock + 6.0, error: false, open_path: Some(path.clone()) });
                let _ = open::that_detached(&path);
                tx.wake_after(std::time::Duration::from_secs_f64(6.05));
            }
            Event::Pending(n) => self.pending_ops = n,
        }
        reload
    }

    /// Starts downloading the open conversation's images (once per URL per session).
    pub fn request_images(&mut self) {
        if self.settings.str("reading.loadImages") != "always" {
            return;
        }
        let mut wanted = Vec::new();
        for m in &self.thread {
            let Some(doc) = m.body.as_ref().and_then(|b| b.doc.as_ref()) else { continue };
            for url in &doc.images {
                if !self.images.contains_key(url) {
                    wanted.push((url.clone(), m.summary.id));
                }
            }
        }
        // Keep memory bounded: forget decoded images once the session has seen a lot of them.
        if self.images.len() > 400 {
            self.images.retain(|_, st| !matches!(st, ImageState::Ready(_)));
        }
        for (url, message) in wanted {
            if self.demo {
                // No engine in demo mode: fetch right here (demo only; blocks briefly).
                let st = match (envoy_core::engine::fetch_image(&url, None), &self.image_cache) {
                    (Ok((w, h, rgba)), Some(cache)) => ImageState::Ready(cache.rgba(w, h, rgba)),
                    _ => ImageState::Failed,
                };
                self.images.insert(url, st);
                continue;
            }
            self.images.insert(url.clone(), ImageState::Loading);
            self.send(Command::FetchImage { url, message });
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast_seq += 1;
        let text = text.into();
        let secs = if error { 8.0 } else { 4.0 };
        self.toasts.push(Toast { id: self.toast_seq, text, undo: None, until: self.clock + secs, error, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    /// Re-reads the list and the open conversation after the store changed.
    pub fn reload(&mut self) {
        self.request_list();
        if let Some(id) = self.thread_for.or(self.open) {
            self.thread_req += 1;
            self.send(Command::Open { req: self.thread_req, id, conversations: self.conversations() });
        }
    }

    pub fn request_list(&mut self) {
        if self.demo {
            let q = envoy_core::store::Query::parse(&self.applied_query);
            let words: Vec<String> = q.text.iter().map(|w| w.to_lowercase()).collect();
            self.list = crate::demo::list_for(&self.view)
                .into_iter()
                .filter(|m| {
                    let hay = format!("{} {} {} {}", m.subject, m.preview, m.from.name, m.from.email).to_lowercase();
                    words.iter().all(|w| hay.contains(w.as_str()))
                        && q.from.iter().all(|f| format!("{} {}", m.from.name, m.from.email).to_lowercase().contains(f.as_str()))
                        && q.unread.is_none_or(|u| u == m.thread_unread)
                        && q.starred.is_none_or(|st| st == m.flagged())
                        && (!q.attachment || m.has_attachments)
                })
                .collect();
            self.list_loaded = true;
            return;
        }
        self.list_req += 1;
        self.send(Command::List {
            req: self.list_req,
            view: self.view.clone(),
            query: self.applied_query.clone(),
            conversations: self.conversations(),
            limit: 3000,
        });
    }

    // ── navigation ──

    pub fn go(&mut self, view: View) {
        self.screen = Screen::Mail;
        if self.view != view {
            self.view = view.clone();
            self.list.clear();
            self.list_loaded = false;
            self.close_message();
        }
        if self.demo {
            self.list = crate::demo::list_for(&self.view);
            self.list_loaded = true;
        }
        self.request_list();
        self.send(Command::Watch { view });
    }

    pub fn go_role(&mut self, role: Role) {
        let account = if self.accounts.len() == 1 { self.accounts.first().map(|a| a.id.clone()) } else { None };
        let unified = self.settings.bool("inbox.unifiedInbox") || self.accounts.len() <= 1;
        let account = if unified { account } else { self.accounts.first().map(|a| a.id.clone()) };
        if role == Role::Flagged {
            let has_folder = self.mailboxes.values().flatten().any(|m| m.role == Role::Flagged);
            if !has_folder {
                self.go(View::Starred { account });
                return;
            }
        }
        self.go(View::Role { account, role });
    }

    pub fn view_title(&self) -> String {
        match &self.view {
            View::Role { account: None, role: Role::Inbox } if self.accounts.len() > 1 => "All inboxes".into(),
            View::Role { role, .. } => role.label().into(),
            View::Starred { .. } => "Starred".into(),
            View::Mailbox { account, mailbox } => self
                .mailboxes
                .get(account)
                .and_then(|m| m.iter().find(|m| &m.name == mailbox))
                .map_or_else(|| mailbox.clone(), |m| m.display.clone()),
        }
    }

    pub fn open_message(&mut self, id: MessageId) {
        if self.open == Some(id) && !self.thread.is_empty() {
            return;
        }
        self.open = Some(id);
        self.thread.clear();
        self.thread_for = None;
        self.show_quoted.clear();
        self.thread_req += 1;
        self.send(Command::Open { req: self.thread_req, id, conversations: self.conversations() });
        if self.demo {
            self.thread = crate::demo::thread_for(id);
            self.thread_for = Some(id);
            self.expanded = self.thread.iter().filter(|m| !m.summary.seen()).map(|m| m.summary.id).collect();
            if let Some(last) = self.thread.last() {
                self.expanded.insert(last.summary.id);
            }
            self.request_images();
        }
        let unread = self.list.iter().find(|s| s.id == id).is_some_and(|s| s.thread_unread);
        if unread {
            match self.settings.str("reading.markAsRead").as_str() {
                "immediately" => self.mark_read(id),
                "delay" => self.mark_read_at = Some((id, self.clock + self.settings.f64("reading.markAsReadDelay"))),
                _ => {}
            }
        }
    }

    fn mark_read(&mut self, id: MessageId) {
        self.mark_read_at = None;
        if let Some(s) = self.list.iter_mut().find(|s| s.id == id) {
            s.flags |= flags::SEEN;
            s.thread_unread = false;
        }
        self.send(Command::SetFlags { ids: vec![id], add: flags::SEEN, remove: 0, thread: self.conversations() });
        if self.demo {
            for m in self.thread.iter_mut() {
                m.summary.flags |= flags::SEEN;
            }
        }
    }

    pub fn close_message(&mut self) {
        self.open = None;
        self.thread.clear();
        self.thread_for = None;
        self.mark_read_at = None;
    }

    fn index_of_open(&self) -> Option<usize> {
        let id = self.open?;
        self.list.iter().position(|s| s.id == id)
    }

    pub fn step(&mut self, delta: isize, tx: &mut Tx) {
        if self.list.is_empty() {
            return;
        }
        let next = match self.index_of_open() {
            Some(i) => (i as isize + delta).clamp(0, self.list.len() as isize - 1) as usize,
            None => 0,
        };
        let id = self.list[next].id;
        self.open_message(id);
        tx.scroll_into_view(row_key(id));
    }

    /// Ticks timers from `poll`: delayed mark-read, toast expiry, settings file changes.
    pub fn tick(&mut self, clock: f64, tx: &mut Tx) -> bool {
        self.clock = clock;
        let mut changed = false;
        if let Some((id, at)) = self.mark_read_at {
            if clock >= at {
                if self.open == Some(id) {
                    self.mark_read(id);
                }
                self.mark_read_at = None;
                changed = true;
            } else {
                tx.wake_after(std::time::Duration::from_secs_f64(at - clock));
            }
        }
        let before = self.toasts.len();
        self.toasts.retain(|t| t.until > clock);
        changed |= before != self.toasts.len();
        if let Some(next) = self.toasts.iter().map(|t| t.until).reduce(f64::min) {
            tx.wake_after(std::time::Duration::from_secs_f64((next - clock).max(0.05)));
        }
        if !self.demo && clock - self.settings_checked_at >= 1.5 {
            self.settings_checked_at = clock;
            if self.settings.changed_on_disk() {
                let fresh = Settings::load();
                self.apply_settings(fresh);
                self.toast("Settings reloaded from settings.json", false);
                changed = true;
            }
        }
        if !self.demo {
            tx.wake_after(std::time::Duration::from_secs(2));
        }
        changed
    }

    pub fn apply_settings(&mut self, s: Settings) {
        let conv_changed = s.bool("inbox.conversationView") != self.settings.bool("inbox.conversationView");
        self.list_width = s.f32("inbox.listWidth");
        self.list_height = s.f32("inbox.listHeight");
        self.keys = parse_keys(&s);
        self.settings = s;
        self.settings_ui.edits.clear();
        self.send(Command::Settings(Box::new(self.settings.clone())));
        if conv_changed {
            self.reload();
        }
    }

    /// Changes one setting from the Settings screen and writes settings.json.
    pub fn set_setting(&mut self, key: &str, v: Value) {
        let mut s = self.settings.clone();
        match s.set(key, v) {
            Ok(()) => {
                if !self.demo
                    && let Err(e) = s.save()
                {
                    self.toast(format!("Couldn't save settings: {e}"), true);
                }
                let keep_edits = std::mem::take(&mut self.settings_ui.edits);
                self.apply_settings(s);
                self.settings_ui.edits = keep_edits;
            }
            Err(e) => self.toast(e, true),
        }
    }

    pub fn reset_setting(&mut self, key: &str) {
        let mut s = self.settings.clone();
        s.reset(key);
        if !self.demo {
            let _ = s.save();
        }
        self.settings_ui.edits.remove(key);
        self.apply_settings(s);
    }

    // ── actions on messages ──

    fn after_remove(&mut self, removed: &[MessageId], tx: &mut Tx) {
        let idx = self.index_of_open();
        let was_open = self.open.is_some_and(|o| removed.contains(&o));
        self.list.retain(|s| !removed.contains(&s.id));
        if !was_open {
            return;
        }
        let policy = self.settings.str("behavior.afterRemove");
        self.close_message();
        let Some(i) = idx else { return };
        let pick = match policy.as_str() {
            "next" => self.list.get(i).or(self.list.last()).map(|s| s.id),
            "previous" => i.checked_sub(1).and_then(|j| self.list.get(j)).or(self.list.first()).map(|s| s.id),
            _ => None,
        };
        if let Some(id) = pick
            && self.settings.str("inbox.readingPane") != "off"
        {
            self.open_message(id);
            tx.scroll_into_view(row_key(id));
        }
    }

    pub fn archive(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        if ids.is_empty() {
            return;
        }
        let label = if ids.len() == 1 { "Archived".to_owned() } else { format!("Archived {}", ids.len()) };
        self.move_away(ids, MoveTarget::Archive, label, tx);
    }

    fn role_of(&self, account: &str, mailbox: &str) -> Role {
        self.mailboxes.get(account).and_then(|m| m.iter().find(|m| m.name == mailbox)).map_or(Role::Other, |m| m.role)
    }

    /// Moves messages off the list at once and tells the engine. While the undo window lasts,
    /// the toast offers Undo; deleting from Trash is for good and offers none.
    fn move_away(&mut self, ids: Vec<MessageId>, to: MoveTarget, label: String, tx: &mut Tx) {
        let first = self.list.iter().find(|s| ids.contains(&s.id));
        let in_trash = match first {
            Some(r) => self.role_of(&r.account, &r.mailbox) == Role::Trash,
            None => matches!(self.view, View::Role { role: Role::Trash, .. }),
        };
        let forever = to == MoveTarget::Trash && in_trash;
        let window = self.settings.u32("behavior.undoSeconds");
        let key = (window > 0 && !forever).then(|| self.next_undo_key());
        let snapshot = key.map(|key| self.snapshot(key, &ids));
        self.send(Command::Move { ids: ids.clone(), to, thread: self.conversations(), undo: key });
        self.after_remove(&ids, tx);
        let label = if forever { "Deleted forever".to_owned() } else { label };
        let Some(snapshot) = snapshot else {
            self.toast(label, false);
            return;
        };
        self.toast_seq += 1;
        let undo = Undo::Move(Box::new(snapshot));
        self.toasts.push(Toast { id: self.toast_seq, text: label, undo: Some(undo), until: self.clock + window as f64, error: false, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        tx.wake_after(std::time::Duration::from_secs_f64(window as f64 + 0.05));
    }

    /// What undo needs to put these rows back on screen.
    fn snapshot(&self, key: i64, ids: &[MessageId]) -> MovedAway {
        MovedAway {
            key,
            view: self.view.clone(),
            query: self.applied_query.clone(),
            rows: self.list.iter().filter(|s| ids.contains(&s.id)).cloned().collect(),
            open: self.open.filter(|o| ids.contains(o)),
        }
    }

    /// A key for [`Command::Move`]'s undo: microseconds since the epoch, so keys of held ops
    /// left from a previous run never collide with this run's.
    fn next_undo_key(&mut self) -> i64 {
        let now = chrono::Utc::now().timestamp_micros();
        self.undo_key = now.max(self.undo_key + 1);
        self.undo_key
    }

    /// Undoes the newest action whose toast still offers Undo. False when there is none.
    pub fn undo_last(&mut self, tx: &mut Tx) -> bool {
        match self.toasts.iter().rev().find(|t| t.undo.is_some()).map(|t| t.id) {
            Some(id) => {
                self.undo_toast(id, tx);
                true
            }
            None => false,
        }
    }

    /// Runs a toast's Undo and dismisses it.
    pub fn undo_toast(&mut self, id: u64, tx: &mut Tx) {
        let Some(i) = self.toasts.iter().position(|t| t.id == id) else { return };
        let toast = self.toasts.remove(i);
        match toast.undo {
            Some(Undo::Send(op)) => self.send(Command::UndoSend { op }),
            Some(Undo::Move(m)) => self.restore(*m, tx),
            None => {}
        }
    }

    /// Puts moved messages back where they were, right away; the engine restores its copy
    /// and the reload that follows confirms it.
    fn restore(&mut self, m: MovedAway, tx: &mut Tx) {
        self.send(Command::UndoMove { undo: m.key });
        if !self.restore_rows(&m) {
            return;
        }
        if let Some(id) = m.open {
            self.open_message(id);
            tx.scroll_into_view(row_key(id));
        }
    }

    /// Re-inserts moved rows into the list if it still shows what they were moved from (same
    /// view, same search). False when it doesn't (the engine's reload will sort it out).
    fn restore_rows(&mut self, m: &MovedAway) -> bool {
        if self.view != m.view || self.applied_query != m.query || self.screen != Screen::Mail {
            return false;
        }
        for row in &m.rows {
            if self.list.iter().any(|s| s.id == row.id) {
                continue;
            }
            // Newest first, like the list.
            let at = self.list.iter().position(|s| s.date < row.date).unwrap_or(self.list.len());
            self.list.insert(at, row.clone());
        }
        true
    }

    pub fn delete(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        if ids.is_empty() {
            return;
        }
        if self.settings.bool("behavior.confirmDelete") {
            self.confirm_delete = Some(ids);
            return;
        }
        self.delete_now(ids, tx);
    }

    pub fn delete_now(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        self.confirm_delete = None;
        self.move_away(ids, MoveTarget::Trash, "Moved to Trash".into(), tx);
    }

    pub fn move_to(&mut self, ids: Vec<MessageId>, to: MoveTarget, tx: &mut Tx) {
        let label = match &to {
            MoveTarget::Junk => "Marked as spam",
            MoveTarget::Inbox => "Moved to Inbox",
            _ => "Moved",
        };
        self.move_away(ids, to, label.into(), tx);
    }

    pub fn toggle_star(&mut self, id: MessageId) {
        let starred = self.list.iter().find(|s| s.id == id).map(|s| s.flagged()).or_else(|| self.thread.iter().find(|m| m.summary.id == id).map(|m| m.summary.flagged()));
        let starred = starred.unwrap_or(false);
        for s in self.list.iter_mut().filter(|s| s.id == id) {
            if starred { s.flags &= !flags::FLAGGED } else { s.flags |= flags::FLAGGED }
        }
        for m in self.thread.iter_mut().filter(|m| m.summary.id == id) {
            if starred { m.summary.flags &= !flags::FLAGGED } else { m.summary.flags |= flags::FLAGGED }
        }
        let (add, remove) = if starred { (0, flags::FLAGGED) } else { (flags::FLAGGED, 0) };
        // Unstarring a conversation clears every star in it; starring marks the one message.
        self.send(Command::SetFlags { ids: vec![id], add, remove, thread: starred && self.conversations() });
    }

    pub fn toggle_read(&mut self, id: MessageId) {
        let unread = self.list.iter().find(|s| s.id == id).map(|s| s.thread_unread).unwrap_or(false);
        for s in self.list.iter_mut().filter(|s| s.id == id) {
            s.thread_unread = !unread;
            if unread { s.flags |= flags::SEEN } else { s.flags &= !flags::SEEN }
        }
        let (add, remove) = if unread { (flags::SEEN, 0) } else { (0, flags::SEEN) };
        self.send(Command::SetFlags { ids: vec![id], add, remove, thread: self.conversations() });
        if !unread && self.open == Some(id) {
            // Marking the open message unread closes it, as Gmail does, so it stays unread.
            self.close_message();
        }
    }

    // ── compose ──

    pub fn signature_for(&self, account: &str) -> String {
        let per = self.accounts.iter().find(|a| a.id == account).map(|a| a.signature.clone()).unwrap_or_default();
        let sig = if per.trim().is_empty() { self.settings.str("compose.signature") } else { per };
        if sig.trim().is_empty() { String::new() } else { format!("\n\n-- \n{sig}") }
    }

    fn default_account(&self) -> String {
        match &self.view {
            View::Mailbox { account, .. } => account.clone(),
            View::Role { account: Some(a), .. } | View::Starred { account: Some(a) } => a.clone(),
            _ => self.accounts.first().map(|a| a.id.clone()).unwrap_or_default(),
        }
    }

    pub fn compose_new(&mut self) {
        let account = self.default_account();
        let sig = self.signature_for(&account);
        let mut to = TextEdit::new("");
        to.focused = true;
        self.compose = Some(Compose {
            account,
            to,
            cc: TextEdit::new(""),
            bcc: TextEdit::new(""),
            subject: TextEdit::new(""),
            body: TextEdit::new(sig),
            show_cc: false,
            in_reply_to: String::new(),
            references: String::new(),
            reply_of: None,
            title: "New message".into(),
            minimized: false,
            error: None,
            from_open: false,
        });
    }

    /// Reply, reply all or forward the given message of the open conversation (default: the newest).
    pub fn compose_reply(&mut self, which: Option<MessageId>, all: bool, forward: bool) {
        let Some(m) = (match which {
            Some(id) => self.thread.iter().find(|m| m.summary.id == id),
            None => self.thread.last(),
        }) else {
            return;
        };
        let s = &m.summary;
        let me: HashSet<String> = self.accounts.iter().map(|a| a.email.to_lowercase()).collect();
        let account = self.accounts.iter().find(|a| a.id == s.account).map(|a| a.id.clone()).unwrap_or_else(|| self.default_account());
        let body = m.body.as_ref();
        let original_text = body
            .and_then(|b| b.text.clone().filter(|t| !t.trim().is_empty()).or_else(|| b.markdown.as_deref().map(fmt::markdown_to_plain)))
            .unwrap_or_else(|| s.preview.clone());
        let base = fmt::base_subject(&s.subject).to_owned();
        let sig = self.signature_for(&account);
        let (to, cc, subject, text, irt, refs) = if forward {
            let header = format!(
                "\n\n---------- Forwarded message ---------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n",
                s.from.formatted(),
                fmt::full_date(s.date),
                s.subject,
                s.to.iter().map(|a| a.formatted()).collect::<Vec<_>>().join(", ")
            );
            (String::new(), String::new(), format!("Fwd: {base}"), format!("{sig}{header}{original_text}"), String::new(), String::new())
        } else {
            let reply_to = body.map(|b| b.reply_to.clone()).filter(|r| !r.is_empty());
            let from_me = me.contains(&s.from.email.to_lowercase());
            let primary: Vec<String> = if from_me {
                s.to.iter().map(|a| a.formatted()).collect()
            } else {
                reply_to.map(|r| r.iter().map(|a| a.formatted()).collect()).unwrap_or_else(|| vec![s.from.formatted()])
            };
            let mut cc_list = Vec::new();
            if all {
                let mut seen: HashSet<String> = me.clone();
                for p in &primary {
                    seen.insert(p.to_lowercase());
                }
                seen.insert(s.from.email.to_lowercase());
                let mut others: Vec<_> = s.to.clone();
                if let Some(b) = body {
                    others.extend(b.cc.iter().cloned());
                }
                for a in others {
                    if seen.insert(a.email.to_lowercase()) {
                        cc_list.push(a.formatted());
                    }
                }
            }
            let quoted = if self.settings.bool("compose.quoteOriginal") {
                format!("\n\nOn {}, {} wrote:\n{}", fmt::full_date(s.date), s.from.display(), fmt::quote(&original_text))
            } else {
                String::new()
            };
            let mid = body.map(|b| b.message_id.clone()).unwrap_or_default();
            let refs = body.map(|b| b.references.clone()).unwrap_or_default();
            let refs = if mid.is_empty() { refs } else if refs.is_empty() { mid.clone() } else { format!("{refs} {mid}") };
            (primary.join(", "), cc_list.join(", "), format!("Re: {base}"), format!("{sig}{quoted}"), mid, refs)
        };
        let mut body_edit = TextEdit::new(text);
        let mut to_edit = TextEdit::new(to.clone());
        if forward || to.is_empty() {
            to_edit.focused = true;
        } else {
            body_edit.focused = true;
            body_edit.anchor = 0;
            body_edit.focus = 0;
        }
        self.compose = Some(Compose {
            account,
            show_cc: !cc.is_empty(),
            to: to_edit,
            cc: TextEdit::new(cc),
            bcc: TextEdit::new(""),
            subject: TextEdit::new(subject.clone()),
            body: body_edit,
            in_reply_to: irt,
            references: refs,
            reply_of: if forward { None } else { Some(s.id) },
            title: subject,
            minimized: false,
            error: None,
            from_open: false,
        });
    }

    pub fn send_compose(&mut self) {
        let Some(c) = &mut self.compose else { return };
        if c.to.text.trim().is_empty() && c.cc.text.trim().is_empty() && c.bcc.text.trim().is_empty() {
            c.error = Some("Add at least one recipient".into());
            return;
        }
        for (label, field) in [("To", &c.to.text), ("Cc", &c.cc.text), ("Bcc", &c.bcc.text)] {
            if let Err(e) = envoy_core::smtp::parse_recipients(field) {
                c.error = Some(format!("{label}: {e}"));
                return;
            }
        }
        let draft = Draft {
            account: c.account.clone(),
            to: c.to.text.clone(),
            cc: c.cc.text.clone(),
            bcc: c.bcc.text.clone(),
            subject: c.subject.text.clone(),
            body: c.body.text.clone(),
            in_reply_to: c.in_reply_to.clone(),
            references: c.references.clone(),
            reply_of: c.reply_of,
        };
        self.compose = None;
        if self.demo {
            self.toast("Sent (demo mode — nothing left your computer)", false);
            return;
        }
        self.send(Command::Send { draft });
    }

    // ── accounts ──

    pub fn google_account(&self, email: &str, name: &str) -> Account {
        Account {
            id: envoy_core::accounts::new_id(email),
            email: email.to_owned(),
            display_name: name.to_owned(),
            auth: AuthKind::GoogleOAuth,
            username: email.to_owned(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            hue: 0.0,
            signature: String::new(),
            oauth_client_id: self.add.client_id.text.trim().to_owned(),
            oauth_client_secret: self.add.client_secret.text.trim().to_owned(),
        }
    }

    pub fn submit_add(&mut self) {
        let f = &mut self.add;
        f.error = None;
        let email = f.email.text.trim().to_owned();
        match f.mode {
            AddMode::Google => {
                if f.client_id.text.trim().is_empty() {
                    f.error = Some("Paste the OAuth client ID from Google Cloud Console (see the steps below), or use an app password instead.".into());
                    return;
                }
                f.busy = true;
                let c = Command::GoogleSignIn { client_id: f.client_id.text.trim().into(), client_secret: f.client_secret.text.trim().into(), hint: email };
                self.send(c);
            }
            AddMode::AppPassword | AddMode::Imap => {
                if !email.contains('@') {
                    f.error = Some("Enter your email address".into());
                    return;
                }
                if f.password.text.is_empty() {
                    f.error = Some("Enter the password".into());
                    return;
                }
                let preset = envoy_core::accounts::preset_for(&email);
                let (ih, ip, sh, sp) = if f.mode == AddMode::Imap || preset.is_none() {
                    let ip = f.imap_port.text.trim().parse().unwrap_or(993);
                    let sp = f.smtp_port.text.trim().parse().unwrap_or(465);
                    (f.imap_host.text.trim().to_owned(), ip, f.smtp_host.text.trim().to_owned(), sp)
                } else {
                    let p = preset.unwrap_or(("", 993, "", 465));
                    (p.0.to_owned(), p.1, p.2.to_owned(), p.3)
                };
                if ih.is_empty() || sh.is_empty() {
                    f.mode = AddMode::Imap;
                    f.error = Some("Envoy doesn't know this provider's servers. Fill in the IMAP and SMTP servers.".into());
                    return;
                }
                f.busy = true;
                let account = Account {
                    id: envoy_core::accounts::new_id(&email),
                    email: email.clone(),
                    display_name: f.name.text.trim().to_owned(),
                    auth: AuthKind::Password,
                    username: email,
                    imap_host: ih,
                    imap_port: ip,
                    smtp_host: sh,
                    smtp_port: sp,
                    hue: 0.0,
                    signature: String::new(),
                    oauth_client_id: String::new(),
                    oauth_client_secret: String::new(),
                };
                let secret = f.password.text.replace(' ', "");
                let secret = if account.is_gmail() { secret } else { f.password.text.clone() };
                self.send(Command::AddAccount { account, secret });
            }
        }
    }

    // ── keyboard ──

    pub fn on_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let typing = tx.focused().is_some_and(|k| k.contains("field") || k.contains("input") || k.contains("compose") || k.contains("search"));
        let action = self.keys.iter().find(|(_, kb)| kb.matches_tx(ev, tx)).map(|(a, _)| *a);
        let Some(action) = action else { return false };
        // Plain-letter shortcuts never fire while typing; chords with a modifier do.
        let plain = !(ev.mods.cmd || ev.mods.ctrl || ev.mods.alt);
        if typing && plain && !matches!(action, Action::Close) {
            return false;
        }
        if self.compose.is_some() {
            match action {
                Action::Send => {
                    self.send_compose();
                    return true;
                }
                Action::Close => {
                    if let Some(c) = &mut self.compose {
                        c.minimized = true;
                    }
                    return true;
                }
                _ if typing => return false,
                _ => {}
            }
        }
        match action {
            Action::Compose => self.compose_new(),
            Action::Reply => {
                if self.open.is_some() {
                    let all = self.settings.bool("compose.defaultReplyAll");
                    self.compose_reply(None, all, false);
                }
            }
            Action::ReplyAll => self.compose_reply(None, true, false),
            Action::Forward => self.compose_reply(None, false, true),
            Action::Archive => {
                if let Some(id) = self.open {
                    self.archive(vec![id], tx);
                }
            }
            Action::Delete => {
                if let Some(id) = self.open {
                    self.delete(vec![id], tx);
                }
            }
            Action::Undo => {
                if !self.undo_last(tx) {
                    return false;
                }
            }
            Action::Star => {
                if let Some(id) = self.thread.last().map(|m| m.summary.id).or(self.open) {
                    self.toggle_star(id);
                }
            }
            Action::ToggleRead => {
                if let Some(id) = self.open {
                    self.toggle_read(id);
                }
            }
            Action::Next => self.step(1, tx),
            Action::Previous => self.step(-1, tx),
            Action::Search => {
                self.screen = Screen::Mail;
                self.search.focused = true;
                self.search.anchor = 0;
                self.search.focus = self.search.text.len();
            }
            Action::Refresh => {
                self.send(Command::SyncNow);
                self.toast("Checking for mail…", false);
            }
            Action::Settings => {
                self.screen = if self.screen == Screen::Settings { Screen::Mail } else { Screen::Settings };
            }
            Action::Send => return false,
            Action::Close => {
                if self.confirm_delete.is_some() {
                    self.confirm_delete = None;
                } else if self.screen != Screen::Mail && !self.accounts.is_empty() {
                    self.screen = Screen::Mail;
                } else if !self.applied_query.is_empty() && self.open.is_none() {
                    self.search = TextEdit::new("");
                    self.applied_query.clear();
                    self.request_list();
                } else if self.open.is_some() {
                    self.close_message();
                } else {
                    return false;
                }
            }
            Action::GoInbox => self.go_role(Role::Inbox),
            Action::GoStarred => self.go_role(Role::Flagged),
            Action::GoSent => self.go_role(Role::Sent),
        }
        true
    }
}

pub fn row_key(id: MessageId) -> String {
    format!("m{id}")
}

/// The label a setting's keybinding shows (`⌘N` / `Ctrl+N`), first chord only.
pub fn key_label(s: &Settings, key: &str) -> String {
    let kb = keybind(&s.str(key));
    kb.chords().first().map(|c| keybind(&c.to_string()).label()).unwrap_or_default()
}

pub fn unread_count(app: &App, account: Option<&str>, role: Role) -> u32 {
    app.mailboxes
        .iter()
        .filter(|(a, _)| account.is_none_or(|x| x == a.as_str()))
        .flat_map(|(_, m)| m.iter())
        .filter(|m| m.role == role)
        .map(|m| m.unread)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keybindings_all_parse() {
        use envoy_core::settings;
        for d in settings::SCHEMA.iter().filter(|d| d.kind == settings::Kind::Keybinding) {
            let spec = Settings::default().str(d.key);
            for part in spec.split(',') {
                assert!(part.trim().parse::<rune::Chord>().is_ok(), "{}: {part:?}", d.key);
            }
        }
    }

    #[test]
    fn undo_puts_rows_back_only_into_the_list_they_left() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let search = |app: &mut App, q: &str| {
            app.applied_query = q.into();
            app.request_list();
        };
        // Archive the one result of a search.
        search(&mut app, "design");
        let row = app.list[0].clone();
        let m = app.snapshot(1, &[row.id]);
        app.list.retain(|s| s.id != row.id);
        // Search for something else, then undo: the row doesn't match, so it isn't added.
        search(&mut app, "receipt");
        let receipts = app.list.clone();
        assert!(!receipts.contains(&row));
        app.restore_rows(&m);
        assert_eq!(app.list, receipts, "undo mustn't add rows the search doesn't match");
        // Undo from the search it was archived in puts it back where it was.
        search(&mut app, "design");
        app.list.retain(|s| s.id != row.id);
        assert!(app.restore_rows(&m));
        assert_eq!(app.list.first(), Some(&row));
    }

    #[test]
    fn a_message_that_comes_back_empty_closes_and_isnt_marked_read() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let _ = app.settings.set("reading.markAsRead", serde_json::json!("delay"));
        let id = app.list.iter().find(|s| s.thread_unread).unwrap().id;
        app.open_message(id);
        assert!(app.mark_read_at.is_some());
        // The engine reloads the open message and finds nothing: it was moved or deleted
        // (say, undo came too late and the move went through).
        let req = app.thread_req;
        app.apply(Event::Thread { req, id, messages: vec![] }, &mut Tx::default());
        assert_eq!(app.open, None, "the reader closes");
        assert_eq!(app.mark_read_at, None, "and nothing gets marked read later");
    }

    #[test]
    fn default_keybindings_dont_collide() {
        let s = Settings::default();
        let mut seen: HashMap<String, &str> = HashMap::new();
        for (_, key) in ACTIONS {
            for part in s.str(key).split(',').map(str::trim) {
                if let Some(other) = seen.insert(part.to_owned(), key) {
                    panic!("{part:?} is bound to both {other} and {key}");
                }
            }
        }
        assert_eq!(seen.get("z"), Some(&"keys.undo"));
    }
}
