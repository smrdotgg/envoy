//! The window's state and everything that changes it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rune::{KeyEvent, Keybind, Mods, TextEdit, Tx};
use rune_kit::{ComboboxOption, ComboboxState, PaneState};
use serde_json::Value;

use envoy_core::accounts::{Account, AuthKind};
use envoy_core::attach;
use envoy_core::engine::{Command, Engine, Event, MoveTarget};
use envoy_core::model::{Category, Draft, DraftAttachment, DraftNotice, FileSource, FileStatus, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};
use envoy_core::settings::Settings;
use envoy_core::store::DraftRev;

use crate::fmt;
use crate::selection::Selection;

mod suggest;
pub use suggest::{Recipients, Suggest};

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
    /// What the title bar says while the subject is empty ([`Compose::shown_title`]).
    pub title: String,
    pub minimized: bool,
    pub error: Option<String>,
    pub from_open: bool,
    /// Its files, in the order they were added, as the store has them (it owns the list: the
    /// composer shows it, and adds and takes off files by commands of their own): attached
    /// ones go with the draft; the others are on their way, or couldn't be attached.
    pub files: Vec<DraftAttachment>,
    /// A file drag from another app is over it.
    pub drop_hover: bool,
    /// The draft this one carries on the text of (a save refused under that id), whose files
    /// it lists, until its saves are confirmed.
    pub files_from: Option<i64>,
    /// What the user should know about it: a draft from the server that can't be edited
    /// without losing something.
    pub notice: Option<String>,
    /// The local draft this composer saves to: an id picked here, unique across runs.
    pub draft_id: i64,
    /// The draft's lifecycle, as the engine last said ([`Event::DraftOpened`]): its saves carry
    /// it, and the store refuses them once the draft has moved on (sent, or gone for good).
    pub lifecycle: i64,
    /// The draft is in the store, or a save of it is on its way: opened from there, or saved.
    pub saved: bool,
    /// The newest save handed to the engine, and the newest it confirmed written (stamps): the
    /// composer says "Draft saved" only once they meet.
    pub submitted: i64,
    pub acked: i64,
    /// The last save failed (it's tried again).
    pub save_error: Option<String>,
    /// Edited since the last save.
    pub dirty: bool,
    /// Edits made, and how many of them [`App::tick`] has timed: a field reports an edit
    /// before the frame that shows it reads the clock.
    pub edits: u64,
    pub timed: u64,
    /// When (app clock) the first unsaved edit and the latest one were seen.
    pub dirty_since: Option<f64>,
    pub edited_at: f64,
    /// Recipient suggestions for the To, Cc or Bcc entry being typed (app/suggest.rs).
    pub suggest: Suggest,
}

/// Seconds after the last keystroke that the composer saves its draft, and the longest it
/// goes without saving while typing continues.
const SAVE_WHEN_IDLE: f64 = 0.8;
const SAVE_AT_LEAST_EVERY: f64 = 3.0;

impl Compose {
    fn new(account: String, draft_id: i64) -> Compose {
        Compose {
            account,
            to: TextEdit::new(""),
            cc: TextEdit::new(""),
            bcc: TextEdit::new(""),
            subject: TextEdit::new(""),
            body: TextEdit::new(""),
            show_cc: false,
            in_reply_to: String::new(),
            references: String::new(),
            reply_of: None,
            title: "New message".into(),
            minimized: false,
            error: None,
            from_open: false,
            files: Vec::new(),
            drop_hover: false,
            files_from: None,
            notice: None,
            draft_id,
            lifecycle: 0,
            saved: false,
            submitted: 0,
            acked: 0,
            save_error: None,
            dirty: false,
            edits: 0,
            timed: 0,
            dirty_since: None,
            edited_at: 0.0,
            suggest: Suggest::default(),
        }
    }

    /// What its title bar says, minimized or not: the subject as it's typed, or, while there's
    /// none, what it was opened as ("New message", "Draft").
    pub fn shown_title(&self) -> &str {
        match self.subject.text.trim() {
            "" if self.title.is_empty() => "New message",
            "" => &self.title,
            subject => subject,
        }
    }

    /// "Draft saved": the store has exactly what the composer shows.
    pub fn shows_saved(&self) -> bool {
        self.saved && !self.dirty && self.save_error.is_none() && self.acked >= self.submitted
    }

    pub fn draft(&self) -> Draft {
        Draft {
            account: self.account.clone(),
            to: self.to.text.clone(),
            cc: self.cc.text.clone(),
            bcc: self.bcc.text.clone(),
            subject: self.subject.text.clone(),
            body: self.body.text.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            reply_of: self.reply_of,
            attachments: self.files.clone(),
        }
    }

    /// What the composer judges the message's size by ([`attach::Limit::over`]): its files'
    /// sizes, and its text's.
    pub fn sizes(&self) -> (Vec<usize>, usize) {
        let files = self.files.iter().filter(|f| !matches!(f.status, FileStatus::Failed(_))).map(|f| f.size).collect();
        (files, self.body.text.len() + self.subject.text.len())
    }

    fn file_mut(&mut self, id: i64) -> Option<&mut DraftAttachment> {
        self.files.iter_mut().find(|f| f.id == id)
    }
}

/// A change to a draft's files handed to the engine and not known to be done: made again, on
/// the spot, if the window closes first ([`App::flush_files`]).
#[derive(Clone, Debug, PartialEq)]
pub enum FileOp {
    Attach { draft: i64, file: DraftAttachment, source: FileSource },
    Detach { draft: i64, file: i64 },
}

/// A file demo mode is pretending to read: attached at `at` (app clock), with what a thread of
/// its own found on disk (demo mode has no engine to read it; the UI thread never touches the
/// disk).
pub struct DemoRead {
    pub at: f64,
    pub draft: i64,
    pub file: i64,
    pub found: std::sync::Arc<std::sync::Mutex<Option<Result<u64, String>>>>,
}

/// A draft save handed to the engine and not yet confirmed written: kept to try again, and to
/// write on the spot if the window closes first.
pub struct PendingSave {
    pub id: i64,
    pub rev: i64,
    pub lifecycle: i64,
    pub draft: Draft,
    pub open: bool,
    /// The draft whose files it lists, if not its own ([`Compose::files_from`]).
    pub files_from: Option<i64>,
    /// Failures so far; after one, it's tried again at `retry_at` (app clock).
    pub failures: u32,
    pub retry_at: Option<f64>,
}

/// A message sent from the composer and not yet confirmed queued: kept to give back if the send
/// fails, and to write as a draft if the window closes first.
pub struct PendingSend {
    pub id: i64,
    pub draft: Draft,
    /// The composer's stamp and lifecycle when it sent.
    pub rev: i64,
    pub lifecycle: i64,
}

/// Seconds before a failed draft save is tried again.
const SAVE_RETRY: f64 = 5.0;

/// What closing the window must still write ([`App::unsaved`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Unsaved {
    /// A draft save, under its stamp and lifecycle (so it never overwrites a newer one, nor
    /// brings back a draft that moved on); `files_from`: the draft whose files it lists, if not
    /// its own.
    Save { id: i64, draft: Draft, open: bool, rev: i64, lifecycle: i64, files_from: Option<i64> },
    /// A message sent from the composer (at `rev`, in `lifecycle`) but not confirmed queued:
    /// kept as a draft unless its send was queued, or went out, after all.
    Unsent { id: i64, draft: Draft, rev: i64, lifecycle: i64 },
}

impl Unsaved {
    fn id(&self) -> i64 {
        match self {
            Unsaved::Save { id, .. } | Unsaved::Unsent { id, .. } => *id,
        }
    }

    /// For the recovery file.
    fn recovered(&self) -> envoy_core::drafts::Recovered {
        match self {
            Unsaved::Save { id, draft, open, rev, lifecycle, files_from } => {
                envoy_core::drafts::Recovered { id: *id, draft: draft.clone(), open: *open, rev: *rev, lifecycle: *lifecycle, unsent: false, files_from: *files_from }
            }
            Unsaved::Unsent { id, draft, rev, lifecycle } => {
                envoy_core::drafts::Recovered { id: *id, draft: draft.clone(), open: true, rev: *rev, lifecycle: *lifecycle, unsent: true, files_from: None }
            }
        }
    }

    /// Its text as a new draft, `id`, saved at `rev`, with the files of the draft it was for:
    /// for one the store refused (its draft moved on without it).
    fn as_new_draft(&self, id: i64, rev: i64) -> Unsaved {
        let (draft, open, files_from) = match self {
            Unsaved::Save { id, draft, open, files_from, .. } => (draft.clone(), *open, files_from.unwrap_or(*id)),
            Unsaved::Unsent { id, draft, .. } => (draft.clone(), true, *id),
        };
        Unsaved::Save { id, draft, open, rev, lifecycle: 0, files_from: Some(files_from) }
    }
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
    /// A discarded composer.
    Discard(Box<Discarded>),
    /// Conversations moved to a category of the split inbox, and the rules that came with it.
    Category(Box<crate::categories::Recategorized>),
}

/// A discarded composer, as it was, to put back.
pub struct Discarded {
    /// The key the engine holds the draft's discard under, if it had a saved draft to discard.
    pub key: Option<i64>,
    pub compose: Compose,
}

/// Enough to put moved messages back on screen at once, before the engine confirms.
pub struct MovedAway {
    /// The key the engine holds the move's ops under.
    pub key: i64,
    /// The view, search and split inbox tab they were removed from; they're put back only if
    /// those are still showing (otherwise the reload after undo decides what belongs in the list).
    pub view: View,
    pub query: String,
    pub category: Option<Category>,
    pub rows: Vec<Summary>,
    /// The open message, if it was one of them.
    pub open: Option<MessageId>,
    /// A composer that closed because its draft was among them.
    pub compose: Option<Box<Compose>>,
    /// Demo mode's drafts discarded from its Drafts list, for Undo to keep again.
    pub demo_drafts: Vec<crate::demo::Kept>,
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
    Attach,
    Close,
    GoInbox,
    GoStarred,
    GoSent,
    Select,
    SelectNext,
    SelectPrevious,
    SelectAll,
    Spam,
    MoveTo,
    Label,
    ShowImportant,
    ShowOther,
    ShowNewsletters,
    ShowNotifications,
    MoveImportant,
    MoveOther,
    MoveNewsletters,
    MoveNotifications,
    Snooze,
}

/// Which picker: Move to… (`v`), Label… (`l`, Gmail) or Snooze… (`b`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Picker {
    Move,
    Label,
    Snooze,
}

/// A picker's options and the note on what it leaves out ([`App::picker_options`]), with what
/// they were built for: the picker, the targets, and the mailboxes' fingerprint.
pub struct PickerCache {
    which: Picker,
    ids: Vec<MessageId>,
    mailboxes: u64,
    /// The split inbox's categories were offered.
    split: bool,
    options: Vec<ComboboxOption>,
    note: Option<String>,
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
    (Action::Attach, "keys.attach"),
    (Action::Close, "keys.close"),
    (Action::GoInbox, "keys.goInbox"),
    (Action::GoStarred, "keys.goStarred"),
    (Action::GoSent, "keys.goSent"),
    (Action::Select, "keys.select"),
    (Action::SelectNext, "keys.selectNext"),
    (Action::SelectPrevious, "keys.selectPrevious"),
    (Action::SelectAll, "keys.selectAll"),
    (Action::Spam, "keys.spam"),
    (Action::MoveTo, "keys.moveTo"),
    (Action::Label, "keys.label"),
    (Action::ShowImportant, "keys.showImportant"),
    (Action::ShowOther, "keys.showOther"),
    (Action::ShowNewsletters, "keys.showNewsletters"),
    (Action::ShowNotifications, "keys.showNotifications"),
    (Action::MoveImportant, "keys.moveImportant"),
    (Action::MoveOther, "keys.moveOther"),
    (Action::MoveNewsletters, "keys.moveNewsletters"),
    (Action::MoveNotifications, "keys.moveNotifications"),
    (Action::Snooze, "keys.snooze"),
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
    /// The split inbox's tab shown ([`App::list_category`]).
    pub category: Category,
    /// A view's unread count per category ([`Event::CategoryCounts`]), for the tabs.
    pub category_counts: Option<(View, [u32; 4])>,
    /// Why an opened message is in its tab ([`Event::Explained`]), for the reader.
    pub explained: Option<(MessageId, Category, String)>,
    /// The list asked for last hasn't all arrived: a change made here meanwhile asks again
    /// ([`App::list_changed_here`]).
    pub list_pending: bool,
    pub list_loaded: bool,
    /// False while `list` is the top of a longer list whose rest is on its way.
    pub list_complete: bool,
    /// Counts the lists the engine sent that replaced `list`.
    pub list_gen: u64,
    /// The pending list request is the user's (a search, another view): its rows show at once
    /// rather than animate in like a reload's.
    pub list_at_once: bool,
    /// j/k waiting for the rest of a list whose first page doesn't reach where they go.
    pub pending_step: isize,
    /// Rows selected to act on together; while there are any, the reader shows them instead
    /// of the open message, and every action applies to them ([`App::targets`]).
    pub selection: Selection,
    /// The Move to… and Label… pickers.
    pub move_picker: ComboboxState,
    pub label_picker: ComboboxState,
    /// The snooze picker (`b`): ready-made times, or one typed ([`App::snooze_options`]).
    pub snooze_picker: ComboboxState,
    /// [`App::picker_options`]'s last answer, and what it was for.
    pub picker_cache: RefCell<Option<PickerCache>>,
    /// The opened row (its newest message in conversation view).
    pub open: Option<MessageId>,
    pub thread: Vec<ThreadMessage>,
    pub thread_req: u64,
    pub thread_for: Option<MessageId>,
    /// Conversations read ahead for the open row's neighbours (`prefetch_neighbours`).
    pub prefetched: HashMap<MessageId, Vec<ThreadMessage>>,
    /// Collapsed / expanded messages in the open conversation.
    pub expanded: HashSet<MessageId>,
    pub show_quoted: HashSet<MessageId>,
    /// Messages whose "layout can't be shown here" note the user dismissed (this session).
    pub layout_note_dismissed: HashSet<MessageId>,
    /// Messages whose remote images the user chose to load ("Load images", this session).
    pub images_shown: HashSet<MessageId>,
    pub search: TextEdit,
    pub applied_query: String,
    pub compose: Option<Compose>,
    /// Draft saves, and sends, not yet confirmed by the engine.
    pub pending_saves: Vec<PendingSave>,
    /// `(draft id, what was sent, when)`.
    pub pending_sends: Vec<PendingSend>,
    pub toasts: Vec<Toast>,
    pub toast_seq: u64,
    /// The last stamp handed out ([`App::next_stamp`]).
    pub last_stamp: i64,
    pub sidebar: PaneState,
    pub collapsed_sections: HashSet<String>,
    pub settings_ui: SettingsUi,
    pub add: AddForm,
    pub mark_read_at: Option<(MessageId, f64)>,
    /// The open message, while it waits to show before the mark-as-read setting applies.
    pub mark_read_when_shown: Option<MessageId>,
    pub clock: f64,
    pub list_drag_start: Option<f32>,
    pub list_width: f32,
    pub list_height: f32,
    pub demo: bool,
    pub settings_checked_at: f64,
    pub confirm_delete: Option<Vec<MessageId>>,
    pub images: HashMap<String, ImageState>,
    pub image_cache: Option<rune::ImageCache>,
    /// Files demo mode is pretending to read.
    pub demo_reads: Vec<DemoRead>,
    /// Drafts written in demo mode, listed in its Drafts.
    pub demo_drafts: crate::demo::Drafts,
    /// Changes to drafts' files the engine may not have made yet.
    pub file_ops: Vec<FileOp>,
    /// The largest message each account's SMTP server takes, as it said ([`Event::SendLimit`]).
    pub send_limits: HashMap<String, u64>,
    /// What `send` was asked to send, for tests.
    #[cfg(test)]
    pub sent: std::cell::RefCell<Vec<String>>,
}

pub fn parse_keys(s: &Settings) -> Vec<(Action, Keybind)> {
    ACTIONS.iter().map(|(a, key)| (*a, keybind(&s.str(key)))).collect()
}

/// The command that saves a draft: one that carries on another's text says whose files it lists.
fn save_command(id: i64, draft: Draft, open: bool, rev: i64, lifecycle: i64, files_from: Option<i64>) -> Command {
    match files_from {
        Some(files_from) => Command::SaveDraftFrom { id, draft, open, rev, lifecycle, files_from },
        None => Command::SaveDraft { id, draft, open, rev, lifecycle },
    }
}

/// Parses a user's keybinding, skipping chords that don't parse (they're flagged in Settings).
pub fn keybind(spec: &str) -> Keybind {
    let valid: Vec<&str> = spec.split(',').map(str::trim).filter(|p| !p.is_empty() && p.parse::<rune::Chord>().is_ok()).collect();
    valid.join(", ").parse().unwrap_or_else(|_| "none".parse().expect("\"none\" is an empty keybind"))
}

impl App {
    pub fn new(demo: bool) -> App {
        #[cfg(test)]
        crate::test_home();
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
            category: Category::Important,
            category_counts: None,
            explained: None,
            list_pending: false,
            list_loaded: false,
            list_complete: true,
            list_gen: 0,
            list_at_once: true,
            pending_step: 0,
            selection: Selection::default(),
            move_picker: ComboboxState::default(),
            label_picker: ComboboxState::default(),
            snooze_picker: ComboboxState::default(),
            picker_cache: RefCell::new(None),
            open: None,
            thread: Vec::new(),
            thread_req: 0,
            thread_for: None,
            prefetched: HashMap::new(),
            expanded: HashSet::new(),
            show_quoted: HashSet::new(),
            layout_note_dismissed: HashSet::new(),
            images_shown: HashSet::new(),
            search: TextEdit::new(""),
            applied_query: String::new(),
            compose: None,
            pending_saves: Vec::new(),
            pending_sends: Vec::new(),
            toasts: Vec::new(),
            toast_seq: 0,
            last_stamp: 0,
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
            mark_read_when_shown: None,
            clock: 0.0,
            list_drag_start: None,
            list_width,
            list_height,
            demo,
            settings_checked_at: 0.0,
            confirm_delete: None,
            images: HashMap::new(),
            image_cache: None,
            demo_reads: Vec::new(),
            demo_drafts: Default::default(),
            file_ops: Vec::new(),
            send_limits: HashMap::new(),
            #[cfg(test)]
            sent: Default::default(),
        }
    }

    pub fn send(&self, c: Command) {
        #[cfg(test)]
        self.sent.borrow_mut().push(format!("{c:?}"));
        if let Some(e) = &self.engine {
            e.send(c);
        }
    }

    pub fn conversations(&self) -> bool {
        self.settings.bool("inbox.conversationView")
    }

    /// The list on screen is one row per conversation, and moving a row moves its conversation:
    /// not in Drafts (two replies drafted in one conversation are two things to finish), nor in
    /// Snoozed (each message is snoozed until its own time, in its own bucket).
    pub fn grouped(&self) -> bool {
        self.conversations() && !matches!(self.view_role(), Some(Role::Drafts | Role::Snoozed))
    }

    // ── engine events ──

    /// Drains engine events; true when anything visible changed.
    pub fn pump(&mut self, tx: &mut Tx) -> bool {
        let events = match &self.engine {
            Some(e) => e.drain(),
            None => return false,
        };
        // A conversation read ahead changes nothing on screen.
        let changed = events.iter().any(|e| !matches!(e, Event::Prefetched { .. }));
        let mut reload = false;
        let before = (self.list_gen, self.thread_for);
        for ev in events {
            reload |= self.apply(ev, tx);
        }
        // A conversation just opened, or the list a search or a new view asked for, shows at
        // once, without fading its paragraphs or rows in: it's what was asked for, and a fade
        // still running when the next key comes keeps every frame waiting for the display's
        // next tick. A reload's changes (new mail, a sync) still animate.
        let thread_arrived = self.thread_for != before.1 && self.thread_for.is_some();
        let list_arrived = self.list_gen != before.0 && self.list_at_once;
        if thread_arrived || list_arrived {
            tx.instant();
        }
        if reload {
            self.reload();
        }
        changed
    }

    /// Applies one engine event. True when what's on screen should be re-read.
    pub(crate) fn apply(&mut self, ev: Event, tx: &mut Tx) -> bool {
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
                    self.selection.clear();
                    reload = true;
                }
                // The first report is the accounts the engine started with, which the list
                // asked for at start was already read with: reloading would read it twice.
                // (Adding or removing one sends AccountAdded or Changed, which reload too.)
                reload |= had;
            }
            Event::Mailboxes { account, mailboxes } => {
                self.mailboxes.insert(account, mailboxes);
            }
            // A refresh only ever shows whole lists: a first page would drop the rows below it,
            // the open one included, from under the reader.
            Event::List { req, items, more } if req == self.list_req && !(more && !self.list_at_once) => {
                self.list = items;
                self.list_loaded = true;
                self.list_complete = !more;
                self.list_gen += 1;
                if !more {
                    self.list_pending = false;
                    // Selected rows that went (moved elsewhere, say) drop out; the rest stay
                    // selected wherever they are now. A first page isn't the whole list.
                    self.selection.retain(&self.list);
                }
                if !more && self.pending_step != 0 {
                    let delta = std::mem::take(&mut self.pending_step);
                    self.step(delta, tx);
                }
            }
            Event::List { .. } => {}
            // Only the newest list's: an older request's, arriving late, would put back what
            // changed since.
            Event::CategoryCounts { req, view, counts } if req == self.list_req => self.category_counts = Some((view, counts)),
            Event::CategoryCounts { .. } => {}
            Event::Explained { id, category, why } => self.explained = Some((id, category, why)),
            Event::Thread { req, id, messages } if req == self.thread_req && messages.is_empty() => {
                // The open message is gone (moved, deleted, or an undo that came too late):
                // close it rather than show an empty reader or mark it read later.
                if self.open == Some(id) || self.thread_for == Some(id) {
                    self.close_message();
                }
            }
            Event::Thread { req, id, messages } => {
                if req == self.thread_req {
                    self.set_thread(id, messages);
                    self.shown(id);
                    self.prefetch_neighbours();
                }
            }
            Event::Prefetched { id, messages } => {
                if self.neighbours().contains(&id) {
                    self.prefetched.insert(id, messages);
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
            Event::Sent { op, undo_secs, draft_id } => {
                // Queued with the draft: the send's copy of it is no longer needed.
                if let Some(d) = draft_id {
                    self.pending_sends.retain(|p| p.id != d);
                    self.pending_saves.retain(|p| p.id != d);
                }
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
                self.selection.clear();
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
            Event::Held { undo, ops: 0 } => {
                // The move had nothing to do (already moving, or already there): no Undo, and
                // the rows the UI took away optimistically come back with a reload, and with
                // them what they took off the sidebar's counts.
                let mut back = Vec::new();
                for t in &mut self.toasts {
                    if matches!(&t.undo, Some(Undo::Move(m)) if m.key == undo)
                        && let Some(Undo::Move(m)) = t.undo.take()
                    {
                        back.extend(m.rows);
                    }
                }
                for (account, mailbox, unread, total) in self.counted(&back) {
                    self.bump_counts(&account, &mailbox, unread, total);
                }
                reload = true;
            }
            Event::Held { .. } => {}
            Event::DraftOpened { id, draft, at, restored, notice } => self.show_draft(id, draft, at, restored, notice),
            Event::DraftDiscarded { id, undo } => self.draft_discarded(id, undo),
            Event::DraftSaved { id, rev } => self.draft_saved(id, rev),
            Event::DraftSaveFailed { id, rev, error } => self.draft_save_failed(id, rev, error),
            Event::DraftSaveRefused { id, rev } => self.draft_save_refused(id, rev),
            Event::FileAttached { draft, file } => self.file_attached(draft, file),
            Event::SendLimit { account, size } => {
                self.send_limits.insert(account, size);
            }
            Event::AttachFailed { draft, file, error } => self.attach_failed(draft, file, error),
            Event::SendFailed { draft_id, error } => self.send_failed(draft_id, error),
            Event::DraftFiles { draft, files } => self.draft_files(draft, files),
            Event::Contacts { req, contacts } => self.contacts_arrived(req, contacts),
        }
        reload
    }

    /// Whether a message's remote images load: the setting allows its sender, or the user
    /// chose "Load images" for it (never when the setting is "never").
    pub fn remote_images(&self, m: &ThreadMessage) -> bool {
        self.settings.remote_images_from(&m.summary.from.email) || self.images_shown.contains(&m.summary.id) && self.settings.str("reading.loadImages") != "never"
    }

    /// The open conversation's images to fetch now: embedded (`cid:`) ones always, remote
    /// ones only where [`Self::remote_images`] allows; each URL once per session.
    pub fn wanted_images(&self) -> Vec<(String, MessageId)> {
        let mut wanted: Vec<(String, MessageId)> = Vec::new();
        for m in &self.thread {
            let Some(doc) = m.body.as_ref().and_then(|b| b.doc.as_ref()) else { continue };
            let remote = self.remote_images(m);
            for url in &doc.images {
                let embedded = url.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("cid:"));
                if (embedded || remote) && !self.images.contains_key(url) && !wanted.iter().any(|(u, _)| u == url) {
                    wanted.push((url.clone(), m.summary.id));
                }
            }
        }
        wanted
    }

    /// "Load images": this message's remote images, this session.
    pub fn load_images(&mut self, id: MessageId) {
        self.images_shown.insert(id);
        self.request_images();
    }

    /// "Always load from …": adds the sender to `reading.imagesFrom` and loads its images.
    pub fn allow_images_from(&mut self, sender: &str) {
        self.allow_sender(sender);
        self.request_images();
    }

    /// Adds `sender` to `reading.imagesFrom` (saved to settings.json outside demo mode).
    fn allow_sender(&mut self, sender: &str) {
        let sender = sender.trim().to_lowercase();
        let mut list = self.settings.list("reading.imagesFrom");
        if !sender.is_empty() && !list.contains(&sender) {
            list.push(sender);
            self.set_setting("reading.imagesFrom", serde_json::json!(list));
        }
    }

    /// Starts downloading the open conversation's images (once per URL per session).
    pub fn request_images(&mut self) {
        let wanted = self.wanted_images();
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

    /// Shows `messages` as the open conversation `id`. Opening it afresh resets which messages
    /// are expanded: the unread ones and the newest, as Gmail does.
    fn set_thread(&mut self, id: MessageId, messages: Vec<ThreadMessage>) {
        let first = self.thread_for != Some(id) || self.thread.is_empty();
        self.thread_for = Some(id);
        if first {
            self.expanded.clear();
            for (i, m) in messages.iter().enumerate() {
                if !m.summary.seen() || i + 1 == messages.len() {
                    self.expanded.insert(m.summary.id);
                }
            }
        }
        self.thread = messages;
        self.request_images();
    }

    /// The rows either side of the open one: what j and k open next.
    fn neighbours(&self) -> Vec<MessageId> {
        let Some(i) = self.index_of_open() else { return Vec::new() };
        [i.checked_sub(1), Some(i + 1)].into_iter().flatten().filter_map(|j| self.list.get(j)).map(|m| m.id).collect()
    }

    /// Reads the open row's neighbours ahead, so j and k show them at once (the North Star's
    /// "opening one is a swap"); forgets conversations read for earlier neighbours.
    fn prefetch_neighbours(&mut self) {
        let near = self.neighbours();
        self.prefetched.retain(|id, _| near.contains(id));
        for id in near {
            if !self.prefetched.contains_key(&id) {
                self.send(Command::Prefetch { id, conversations: self.conversations() });
            }
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
        self.request_list_as(false);
        if let Some(id) = self.thread_for.or(self.open) {
            self.thread_req += 1;
            self.send(Command::Open { req: self.thread_req, id, conversations: self.conversations() });
        }
    }

    /// Asks for the list the user wants to see now (a new view, a search, startup): its top is
    /// shown as soon as it's read, without animating in.
    pub fn request_list(&mut self) {
        self.request_list_as(true);
    }

    /// `at_once`: the user is waiting on a new list ([`App::request_list`]); otherwise the list
    /// on screen is read again ([`App::reload`]) and comes whole, changes animating.
    fn request_list_as(&mut self, at_once: bool) {
        let category = self.list_category();
        if self.demo {
            let q = envoy_core::store::Query { tab: category, ..envoy_core::store::Query::parse(&self.applied_query) };
            let words: Vec<String> = q.text.iter().map(|w| w.to_lowercase()).collect();
            self.list = crate::demo::list_with(&self.view, &self.demo_drafts)
                .into_iter()
                .filter(|m| {
                    let hay = format!("{} {} {} {}", m.subject, m.preview, m.from.name, m.from.email).to_lowercase();
                    words.iter().all(|w| hay.contains(w.as_str()))
                        && q.from.iter().all(|f| format!("{} {}", m.from.name, m.from.email).to_lowercase().contains(f.as_str()))
                        && q.unread.is_none_or(|u| u == m.thread_unread)
                        && q.starred.is_none_or(|st| st == m.flagged())
                        && (!q.attachment || m.has_attachments)
                        && q.categories().is_none_or(|mask| mask & 1 << m.category.code() != 0)
                })
                .collect();
            if self.split_applies() {
                self.category_counts = Some((self.view.clone(), crate::demo::category_counts(&self.view)));
            }
            self.list_loaded = true;
            self.selection.retain(&self.list);
            return;
        }
        self.list_req += 1;
        self.list_pending = true;
        self.list_at_once = at_once;
        self.send(Command::List {
            req: self.list_req,
            view: self.view.clone(),
            query: self.applied_query.clone(),
            conversations: self.grouped(),
            limit: 3000,
            stream: at_once,
            category,
            // The tabs show over a search too, and keep counting.
            counts: self.split_applies(),
        });
    }

    // ── navigation ──

    pub fn go(&mut self, view: View) {
        self.screen = Screen::Mail;
        if self.view != view {
            // Left before its row goes: a message back from a snooze, read, is dealt with.
            self.close_message();
            self.view = view.clone();
            self.list.clear();
            self.list_loaded = false;
            self.selection.clear();
        }
        if self.demo {
            self.list = crate::demo::list_with(&self.view, &self.demo_drafts);
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
        if self.is_draft_row(id) {
            self.open_draft(id);
            return;
        }
        if self.open == Some(id) && !self.thread.is_empty() {
            return;
        }
        if self.open != Some(id) {
            self.unpin_left();
        }
        self.pending_step = 0;
        self.open = Some(id);
        self.thread.clear();
        self.thread_for = None;
        self.show_quoted.clear();
        self.thread_req += 1;
        self.send(Command::Open { req: self.thread_req, id, conversations: self.conversations() });
        // Read ahead: show it now; the Open above brings it fresh a moment later.
        if let Some(messages) = self.prefetched.remove(&id) {
            self.set_thread(id, messages);
        }
        if self.demo {
            self.thread = crate::demo::thread_for(id);
            self.thread_for = Some(id);
            self.explained = self.list.iter().find(|s| s.id == id).map(|s| (id, s.category, crate::demo::why(s.category).to_owned()));
            self.expanded = self.thread.iter().filter(|m| !m.summary.seen()).map(|m| m.summary.id).collect();
            if let Some(last) = self.thread.last() {
                self.expanded.insert(last.summary.id);
            }
            self.request_images();
        }
        // Marked read once it actually shows: the engine may find nothing (it was moved, or a
        // late undo reopened a message that's still on its way out).
        // Not one still on its way here: nothing is done to it until it's there.
        let unread = self.list.iter().find(|s| s.id == id).is_some_and(|s| s.thread_unread && s.flags & flags::ARRIVING == 0);
        self.mark_read_when_shown = unread.then_some(id);
        if !self.thread.is_empty() {
            self.shown(id);
        }
    }

    /// The open message's content arrived: apply the mark-as-read setting, once per open. Not
    /// while rows are selected: the reader shows them instead, and the setting waits for the
    /// message to show ([`App::sync_reading`]).
    fn shown(&mut self, id: MessageId) {
        if self.open != Some(id) || self.mark_read_when_shown != Some(id) || !self.selection.is_empty() {
            return;
        }
        self.mark_read_when_shown = None;
        match self.settings.str("reading.markAsRead").as_str() {
            "immediately" => self.mark_read(id),
            "delay" => self.mark_read_at = Some((id, self.clock + self.settings.f64("reading.markAsReadDelay"))),
            _ => {}
        }
    }

    /// Keeps marking the open message read in step with whether it shows: a selection hides it
    /// (the reader shows the selection), so a delay running for it stops, to start over once it
    /// shows again; and once it does, what waited for it (the delay, or "immediately") happens.
    pub fn sync_reading(&mut self) {
        if !self.selection.is_empty() {
            if let Some((id, _)) = self.mark_read_at.take() {
                self.mark_read_when_shown = Some(id);
            }
            return;
        }
        if let Some(id) = self.mark_read_when_shown
            && self.open == Some(id)
            && self.thread_for == Some(id)
            && !self.thread.is_empty()
        {
            self.shown(id);
        }
    }

    /// Unselects every row: the reader shows the open message again.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.sync_reading();
    }

    fn mark_read(&mut self, id: MessageId) {
        self.mark_read_at = None;
        self.count_read(id, true);
        if let Some(s) = self.list.iter_mut().find(|s| s.id == id) {
            s.flags |= flags::SEEN;
            s.thread_unread = false;
        }
        self.send(Command::SetFlags { ids: vec![id], add: flags::SEEN, remove: 0, thread: self.conversations() });
        self.list_changed_here();
        if self.demo {
            for m in self.thread.iter_mut() {
                m.summary.flags |= flags::SEEN;
            }
        }
    }

    /// Moves the sidebar's counts at once for a change made here. The engine's own counts
    /// replace them a moment later (in demo mode nothing else would).
    fn bump_counts(&mut self, account: &str, mailbox: &str, unread: i64, total: i64) {
        if let Some(m) = self.mailboxes.get_mut(account).and_then(|boxes| boxes.iter_mut().find(|m| m.name == mailbox)) {
            m.unread = (m.unread as i64 + unread).max(0) as u32;
            m.total = (m.total as i64 + total).max(0) as u32;
        }
    }

    /// Counts row `id` read (or unread) in the sidebar, before the engine does: each unread
    /// message of the open conversation when it's that row's, else the row.
    fn count_read(&mut self, id: MessageId, read: bool) {
        let Some(row) = self.list.iter().find(|s| s.id == id) else { return };
        if row.thread_unread != read {
            return;
        }
        let (account, mailbox) = (row.account.clone(), row.mailbox.clone());
        if !read {
            self.bump_counts(&account, &mailbox, 1, 0);
            return;
        }
        let unread: Vec<(String, String)> = if self.thread_for == Some(id) && self.conversations() {
            self.thread.iter().filter(|m| !m.summary.seen()).map(|m| (m.summary.account.clone(), m.summary.mailbox.clone())).collect()
        } else {
            Vec::new()
        };
        if unread.is_empty() {
            self.bump_counts(&account, &mailbox, -1, 0);
        }
        for (account, mailbox) in unread {
            self.bump_counts(&account, &mailbox, -1, 0);
        }
    }

    /// What rows leaving their mailbox take from its counts: (unread, total) as the sidebar
    /// shows them, for an archive or move and for its undo.
    fn counted(&self, rows: &[Summary]) -> Vec<(String, String, i64, i64)> {
        let conversations = self.conversations();
        rows.iter()
            .map(|r| (r.account.clone(), r.mailbox.clone(), i64::from(r.thread_unread), if conversations { i64::from(r.thread_count.max(1)) } else { 1 }))
            .collect()
    }

    /// A change was just made here, optimistically, to rows on screen. A list read before it
    /// and still on its way would show them as they were for a moment (an archived row coming
    /// back, say), so it's asked for again: the engine reads the new one after the change.
    pub(crate) fn list_changed_here(&mut self) {
        if self.list_pending {
            self.request_list_as(self.list_at_once);
        }
    }

    pub fn close_message(&mut self) {
        self.unpin_left();
        self.pending_step = 0;
        self.open = None;
        self.thread.clear();
        self.thread_for = None;
        self.prefetched.clear();
        self.mark_read_at = None;
        self.mark_read_when_shown = None;
    }

    /// The open row is being left: back from a snooze and read now, it's been dealt with, and
    /// isn't pinned atop the inbox any more. It keeps its place until the list is read again
    /// (then it's in its place by date), so j and k go on from where they were.
    fn unpin_left(&mut self) {
        let Some(open) = self.open else { return };
        // Its row, or (gone from the list) its conversation as the reader has it.
        let (woken, unread) = match self.list.iter().find(|s| s.id == open) {
            Some(row) => (row.flags & flags::WOKEN != 0, row.thread_unread),
            None if self.thread_for == Some(open) => (self.thread.iter().any(|m| m.summary.flags & flags::WOKEN != 0), self.thread.iter().any(|m| !m.summary.seen())),
            None => return,
        };
        if !woken || unread {
            return;
        }
        for row in self.list.iter_mut().filter(|s| s.id == open) {
            row.flags &= !flags::WOKEN;
        }
        self.send(Command::SetFlags { ids: vec![open], add: 0, remove: flags::WOKEN, thread: self.conversations() });
    }

    fn index_of_open(&self) -> Option<usize> {
        let id = self.open?;
        self.list.iter().position(|s| s.id == id)
    }

    pub fn step(&mut self, delta: isize, tx: &mut Tx) {
        // A first page whose rest is still coming: if the open row is below it, or the step
        // would run past its end, move once the rest is here (from the open row, not the top
        // or the page's last row). Steps after one that waits wait too, so they keep their order.
        if !self.list_complete && self.open.is_some() {
            let past = match self.index_of_open() {
                Some(i) => i as isize + delta >= self.list.len() as isize,
                None => true,
            };
            if past || self.pending_step != 0 {
                self.pending_step += delta;
                return;
            }
        }
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
        // Stepping through the list with the keyboard moves at once, like a native list: no
        // highlight or scroll animation trailing behind a quick j j j.
        tx.instant();
    }

    /// Ticks timers from `poll`: draft autosave and retries, delayed mark-read, toast expiry,
    /// settings file changes.
    pub fn tick(&mut self, clock: f64, tx: &mut Tx) -> bool {
        self.clock = clock;
        let mut changed = self.finish_demo_reads(clock, tx);
        changed |= self.autosave(clock, tx);
        self.retry_saves(clock, tx);
        self.sync_reading();
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
        // Either changes which rows the list has (one per conversation, or the split inbox's tab).
        let conv_changed = s.bool("inbox.conversationView") != self.settings.bool("inbox.conversationView") || s.bool("inbox.split") != self.settings.bool("inbox.split");
        let images_changed = s.value("reading.loadImages") != self.settings.value("reading.loadImages") || s.value("reading.imagesFrom") != self.settings.value("reading.imagesFrom");
        self.list_width = s.f32("inbox.listWidth");
        self.list_height = s.f32("inbox.listHeight");
        self.keys = parse_keys(&s);
        self.settings = s;
        self.settings_ui.edits.clear();
        self.send(Command::Settings(Box::new(self.settings.clone())));
        if conv_changed {
            self.reload();
        }
        if images_changed {
            self.request_images();
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
        let gone: Vec<Summary> = self.list.iter().filter(|s| removed.contains(&s.id)).cloned().collect();
        for (account, mailbox, unread, total) in self.counted(&gone) {
            self.bump_counts(&account, &mailbox, -unread, -total);
        }
        self.drop_rows(removed, tx);
    }

    /// Takes rows off the list (they left it: moved away, or to another tab of the split inbox),
    /// and opens what `behavior.afterRemove` says if the open one went.
    pub(crate) fn drop_rows(&mut self, removed: &[MessageId], tx: &mut Tx) {
        let idx = self.index_of_open();
        let was_open = self.open.is_some_and(|o| removed.contains(&o));
        self.list.retain(|s| !removed.contains(&s.id));
        self.selection.retain(&self.list);
        if !was_open {
            // The selection may have gone with the rows: the open message shows again.
            self.sync_reading();
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

    pub fn archive(&mut self, mut ids: Vec<MessageId>, tx: &mut Tx) {
        // Drafts aren't archived (they're discarded, or sent).
        ids.retain(|id| !self.is_draft_row(*id));
        if ids.is_empty() {
            return;
        }
        let label = if ids.len() == 1 { "Archived".to_owned() } else { format!("Archived {}", ids.len()) };
        self.move_away(ids, MoveTarget::Archive, label, tx);
    }

    fn role_of(&self, account: &str, mailbox: &str) -> Role {
        self.mailboxes.get(account).and_then(|m| m.iter().find(|m| m.name == mailbox)).map_or(Role::Other, |m| m.role)
    }

    /// What the mailbox (or role) on screen is for; None for Starred.
    pub fn view_role(&self) -> Option<Role> {
        match &self.view {
            View::Role { role, .. } => Some(*role),
            View::Mailbox { account, mailbox } => Some(self.role_of(account, mailbox)),
            View::Starred { .. } => None,
        }
    }

    /// A row of a Drafts list: it opens in the composer, not the reader. Local drafts the server
    /// doesn't have yet have negative ids.
    fn is_draft_row(&self, id: MessageId) -> bool {
        id < 0 || self.list.iter().find(|s| s.id == id).is_some_and(|s| self.role_of(&s.account, &s.mailbox) == Role::Drafts)
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
        let thread = self.grouped();
        self.take_away(ids, label, forever, tx, move |ids, undo| Command::Move { ids, to, thread, undo });
    }

    /// [`App::move_away`]'s part shared with snoozing: off the list at once, `command(ids,
    /// undo key)` to the engine, and a toast with Undo for the undo window (none when
    /// `forever`). Messages the server snoozed itself are left alone: they're its to move.
    fn take_away(&mut self, ids: Vec<MessageId>, label: String, forever: bool, tx: &mut Tx, command: impl FnOnce(Vec<MessageId>, Option<i64>) -> Command) {
        let mut ids = self.settled(ids);
        let before = ids.len();
        ids.retain(|id| !self.snoozed_by_server(*id));
        if ids.len() < before {
            self.toast("Your mail provider snoozed that: change it there", false);
        }
        if ids.is_empty() {
            return;
        }
        let window = self.settings.u32("behavior.undoSeconds");
        let key = (window > 0 && !forever).then(|| self.next_stamp());
        let snapshot = key.map(|key| self.snapshot(key, &ids));
        self.send(command(ids.clone(), key));
        self.after_remove(&ids, tx);
        self.list_changed_here();
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
    pub(crate) fn snapshot(&self, key: i64, ids: &[MessageId]) -> MovedAway {
        MovedAway {
            key,
            view: self.view.clone(),
            query: self.applied_query.clone(),
            category: self.list_category(),
            rows: self.list.iter().filter(|s| ids.contains(&s.id)).cloned().collect(),
            open: self.open.filter(|o| ids.contains(o)),
            compose: None,
            demo_drafts: Vec::new(),
        }
    }

    /// A number no earlier run or earlier call handed out: microseconds since the epoch. For
    /// [`Command::Move`]'s undo keys (held ops left from a previous run never collide with this
    /// run's), draft ids, and the order of draft saves.
    pub(crate) fn next_stamp(&mut self) -> i64 {
        let now = chrono::Utc::now().timestamp_micros();
        self.last_stamp = now.max(self.last_stamp + 1);
        self.last_stamp
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
            Some(Undo::Discard(d)) => self.undo_discard(*d),
            Some(Undo::Category(r)) => self.undo_category(*r, tx),
            None => {}
        }
    }

    /// Puts moved messages back where they were, right away; the engine restores its copy
    /// and the reload that follows confirms it.
    fn restore(&mut self, mut m: MovedAway, tx: &mut Tx) {
        self.send(Command::UndoMove { undo: m.key });
        self.demo_drafts.put_back(std::mem::take(&mut m.demo_drafts));
        for (account, mailbox, unread, total) in self.counted(&m.rows) {
            self.bump_counts(&account, &mailbox, unread, total);
        }
        if let Some(c) = m.compose.take() {
            self.reopen(*c);
        }
        let shown = self.restore_rows(&m);
        self.list_changed_here();
        if !shown {
            return;
        }
        if let Some(id) = m.open {
            self.open_message(id);
            tx.scroll_into_view(row_key(id));
        }
    }

    /// Re-inserts moved rows into the list if it still shows what they were moved from (same
    /// view, same search). False when it doesn't (the engine's reload will sort it out).
    pub(crate) fn restore_rows(&mut self, m: &MovedAway) -> bool {
        if self.view != m.view || self.applied_query != m.query || self.list_category() != m.category || self.screen != Screen::Mail {
            return false;
        }
        for row in &m.rows {
            if self.list.iter().any(|s| s.id == row.id) {
                continue;
            }
            // Where the store will put it: newest first, then the newer id (store/list.rs ORDER).
            let at = self.list.iter().position(|s| (s.date, s.id) < (row.date, row.id)).unwrap_or(self.list.len());
            self.list.insert(at, row.clone());
        }
        true
    }

    pub fn delete(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        if ids.is_empty() {
            return;
        }
        // Drafts are discarded (Undo stands in for the confirmation).
        if ids.iter().all(|id| self.is_draft_row(*id)) {
            self.discard_rows(ids, tx);
            return;
        }
        if self.settings.bool("behavior.confirmDelete") {
            self.confirm_delete = Some(ids);
            return;
        }
        self.delete_now(ids, tx);
    }

    /// Deletes rows of a Drafts list: their drafts are discarded here and on the server (a draft
    /// only the server has goes to Trash), off the list at once, with Undo. The composer's own
    /// draft is discarded as the composer.
    fn discard_rows(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        let mine = self.compose.as_ref().map(|c| envoy_core::drafts::list_id(c.draft_id));
        let (open, rows): (Vec<MessageId>, Vec<MessageId>) = ids.into_iter().partition(|id| Some(*id) == mine);
        if !open.is_empty() {
            self.discard_compose(tx);
        }
        if rows.is_empty() {
            return;
        }
        let window = self.settings.u32("behavior.undoSeconds");
        let key = (window > 0).then(|| self.next_stamp());
        let mut snapshot = key.map(|key| self.snapshot(key, &rows));
        for row in &rows {
            self.send(Command::DiscardDraftRow { row: *row, undo: key });
        }
        // No engine in demo mode: its drafts leave the session's, and Undo keeps them again.
        let taken = self.demo_drafts.take_rows(&rows);
        if let Some(s) = &mut snapshot {
            s.demo_drafts = taken;
        }
        self.after_remove(&rows, tx);
        let label = if rows.len() == 1 { "Draft discarded".to_owned() } else { format!("{} drafts discarded", rows.len()) };
        let Some(snapshot) = snapshot else {
            self.toast(label, false);
            return;
        };
        self.toast_seq += 1;
        self.toasts.push(Toast { id: self.toast_seq, text: label, undo: Some(Undo::Move(Box::new(snapshot))), until: self.clock + window as f64, error: false, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        tx.wake_after(std::time::Duration::from_secs_f64(window as f64 + 0.05));
    }

    /// A draft was discarded from the Drafts list while open in the composer: the composer
    /// closes, and the discard's Undo brings it back.
    fn draft_discarded(&mut self, id: i64, undo: Option<i64>) {
        // Discarded: a save still waiting mustn't bring it back when the window closes.
        self.pending_saves.retain(|p| p.id != id);
        if self.compose.as_ref().is_none_or(|c| c.draft_id != id) {
            return;
        }
        let Some(c) = self.compose.take() else { return };
        let toast = self.toasts.iter_mut().find_map(|t| match &mut t.undo {
            Some(Undo::Move(m)) if Some(m.key) == undo => Some(m),
            _ => None,
        });
        if let Some(m) = toast {
            m.compose = Some(Box::new(c));
        }
    }

    pub fn delete_now(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        self.confirm_delete = None;
        self.move_away(ids, MoveTarget::Trash, "Moved to Trash".into(), tx);
    }

    pub fn move_to(&mut self, ids: Vec<MessageId>, to: MoveTarget, tx: &mut Tx) {
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        let label = match &to {
            MoveTarget::Junk if n == 1 => "Marked as spam".to_owned(),
            MoveTarget::Junk => format!("Marked {n} as spam"),
            MoveTarget::Archive if n == 1 => "Archived".to_owned(),
            MoveTarget::Archive => format!("Archived {n}"),
            MoveTarget::Trash if n == 1 => "Moved to Trash".to_owned(),
            MoveTarget::Trash => format!("Moved {n} to Trash"),
            MoveTarget::Inbox if n == 1 => "Moved to Inbox".to_owned(),
            MoveTarget::Inbox => format!("Moved {n} to Inbox"),
            MoveTarget::Mailbox(name) => {
                let display = self.mailbox_display(&ids, name);
                if n == 1 { format!("Moved to {display}") } else { format!("Moved {n} to {display}") }
            }
        };
        self.move_away(ids, to, label, tx);
    }

    // ── snoozes ──

    /// When the messages of `mailbox` wake, if it's one of Envoy's snooze buckets.
    pub fn wake_of(&self, account: &str, mailbox: &str) -> Option<i64> {
        self.mailboxes.get(account)?.iter().find(|m| m.name == mailbox)?.wake
    }

    /// Row `id` is in the server's own snoozed mailbox (Snoozed, but not one of Envoy's buckets):
    /// shown, never moved (envoy_core::snooze).
    pub fn snoozed_by_server(&self, id: MessageId) -> bool {
        self.list.iter().find(|s| s.id == id).is_some_and(|s| self.role_of(&s.account, &s.mailbox) == Role::Snoozed && self.wake_of(&s.account, &s.mailbox).is_none())
    }

    /// Opens the snooze picker for the targets (not drafts), its field ready for a typed time.
    pub fn open_snooze_picker(&mut self) {
        let ids: Vec<MessageId> = self.targets().into_iter().filter(|id| !self.is_draft_row(*id)).collect();
        if ids.is_empty() {
            if !self.targets().is_empty() {
                self.toast("Drafts can't be snoozed", false);
            }
            return;
        }
        let state = &mut self.snooze_picker;
        state.open = true;
        state.search.clear();
        state.search.focused = true;
        state.active = None;
    }

    /// What the snooze picker offers now: the time typed in its field, if it reads as one;
    /// Later today, Tonight, Tomorrow, This weekend, Next week (those still ahead); and a note
    /// on typing one. Each id ends with its time (Unix seconds).
    pub fn snooze_options(&self) -> Vec<ComboboxOption> {
        let now = chrono::Local::now();
        let mut out = Vec::new();
        let typed = self.snooze_picker.search.text.trim();
        if !typed.is_empty() {
            // Its detail is what was typed, so it's never filtered out by it.
            let mut o = match envoy_core::snooze::parse_when(typed, &now) {
                Some(at) => ComboboxOption::new(format!("typed:{at}"), fmt::snooze_until(at)),
                None => ComboboxOption { disabled: true, ..ComboboxOption::new("typed:none", "Not a time ahead that Envoy can read") },
            };
            o.detail = typed.to_owned();
            out.push(o);
        }
        for p in envoy_core::snooze::presets(&now) {
            let mut o = ComboboxOption::new(format!("{}:{}", p.id, p.at), p.label);
            o.detail = fmt::snooze_until(p.at);
            out.push(o);
        }
        out.push(ComboboxOption { disabled: true, detail: "type it: fri 9am, in 2 days".into(), ..ComboboxOption::new("pick", "Pick date & time…") });
        out
    }

    /// The snooze picker's choice.
    pub fn pick_snooze(&mut self, option: &str, tx: &mut Tx) {
        if let Some(at) = option.rsplit(':').next().and_then(|t| t.parse::<i64>().ok()) {
            self.snooze(self.targets(), at, tx);
        }
    }

    /// Snoozes messages until `until` (Unix seconds): off the list at once, to their buckets on
    /// the server once the undo window is over, back atop the inbox at that time. In Snoozed,
    /// a new time for them.
    pub fn snooze(&mut self, mut ids: Vec<MessageId>, until: i64, tx: &mut Tx) {
        ids.retain(|id| !self.is_draft_row(*id));
        if ids.is_empty() {
            return;
        }
        let when = fmt::snooze_until(until);
        let label = if ids.len() == 1 { format!("Snoozed until {when}") } else { format!("Snoozed {} until {when}", ids.len()) };
        let thread = self.grouped();
        self.take_away(ids, label, false, tx, move |ids, undo| Command::Snooze { ids, until, thread, undo });
    }

    /// Takes snoozed messages back to the inbox now.
    pub fn unsnooze(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        if ids.is_empty() {
            return;
        }
        let label = if ids.len() == 1 { "Unsnoozed".to_owned() } else { format!("Unsnoozed {}", ids.len()) };
        self.move_away(ids, MoveTarget::Inbox, label, tx);
    }

    /// Every account's snooze mailboxes (buckets, and the server's own), for the sidebar.
    pub fn has_snoozed(&self, account: Option<&str>) -> bool {
        self.mailboxes.iter().filter(|(a, _)| account.is_none_or(|x| x == a.as_str())).flat_map(|(_, m)| m.iter()).any(|m| m.role == Role::Snoozed)
    }

    // ── several at once ──

    /// What an action applies to: the selected rows, else the open one.
    pub fn targets(&self) -> Vec<MessageId> {
        if self.selection.is_empty() { self.open.into_iter().collect() } else { self.selection.ids(&self.list) }
    }

    /// Row `id` is shown where a move or label not on the server yet is taking it
    /// ([`flags::ARRIVING`]): its undo window is still open, or it's waiting to go.
    pub fn is_arriving(&self, id: MessageId) -> bool {
        self.list.iter().find(|s| s.id == id).map_or_else(|| self.thread.iter().any(|m| m.summary.id == id && m.summary.flags & flags::ARRIVING != 0), |s| s.flags & flags::ARRIVING != 0)
    }

    /// `ids` but the rows still on their way where they show ([`App::is_arriving`]): nothing is
    /// done to those until they're there (Undo takes them back meanwhile). If that leaves
    /// nothing to act on, says why.
    pub(crate) fn settled(&mut self, mut ids: Vec<MessageId>) -> Vec<MessageId> {
        let before = ids.len();
        ids.retain(|id| !self.is_arriving(*id));
        if ids.is_empty() && before > 0 {
            let what = if before == 1 { "That's" } else { "Those are" };
            self.toast(format!("{what} still on the way here: undo the move, or try again in a moment"), false);
        }
        ids
    }

    /// The rows (or the open message's summary) of these ids, in their order: one pass over the
    /// list, however many there are (a select-all of thousands).
    pub(crate) fn rows_of(&self, ids: &[MessageId]) -> Vec<&Summary> {
        let wanted: HashSet<MessageId> = ids.iter().copied().collect();
        let mut found: HashMap<MessageId, &Summary> = self.list.iter().filter(|s| wanted.contains(&s.id)).map(|s| (s.id, s)).collect();
        for m in &self.thread {
            if wanted.contains(&m.summary.id) {
                found.entry(m.summary.id).or_insert(&m.summary);
            }
        }
        ids.iter().filter_map(|id| found.get(id).copied()).collect()
    }

    /// The distinct `(account, mailbox)`s these rows are in.
    fn places<'a>(rows: &[&'a Summary]) -> HashSet<(&'a str, &'a str)> {
        rows.iter().map(|r| (r.account.as_str(), r.mailbox.as_str())).collect()
    }

    /// What the sidebar calls mailbox `name` of the account these messages are in.
    fn mailbox_display(&self, ids: &[MessageId], name: &str) -> String {
        let account = self.rows_of(ids).first().map(|r| r.account.clone()).unwrap_or_default();
        self.mailboxes.get(&account).and_then(|m| m.iter().find(|m| m.name == name)).map_or_else(|| name.to_owned(), |m| m.display.clone())
    }

    /// Every one of them is in Spam: `!` and the Spam button take them back to the inbox.
    pub fn all_in_junk(&self, ids: &[MessageId]) -> bool {
        let places = Self::places(&self.rows_of(ids));
        !places.is_empty() && places.iter().all(|(account, mailbox)| self.role_of(account, mailbox) == Role::Junk)
    }

    /// Spam, or "not spam" for messages already in Spam.
    pub fn spam(&mut self, ids: Vec<MessageId>, tx: &mut Tx) {
        let to = if self.all_in_junk(&ids) { MoveTarget::Inbox } else { MoveTarget::Junk };
        self.move_to(ids, to, tx);
    }

    /// Every one of them is in a Gmail account, where labels are a thing.
    pub fn all_gmail(&self, ids: &[MessageId]) -> bool {
        let accounts: HashSet<&str> = self.rows_of(ids).iter().map(|r| r.account.as_str()).collect();
        !accounts.is_empty() && accounts.iter().all(|id| self.accounts.iter().any(|a| a.id == *id && a.is_gmail()))
    }

    /// Selects or unselects a row (x on the keyboard's row, a row's checkbox, mod-click).
    pub fn toggle_selected(&mut self, id: MessageId) {
        self.selection.toggle(id);
        self.sync_reading();
    }

    /// A press on a row: shift selects the range to it, mod adds or takes it from the
    /// selection; a plain press opens it, ending any selection.
    pub fn press_row(&mut self, id: MessageId, mods: Mods, tx: &mut Tx) {
        if mods.shift {
            let from = self.selection.cursor().or(self.open);
            self.selection.select_range(&self.list, from, id);
        } else if mods.cmd {
            if self.selection.is_empty()
                && let Some(open) = self.open.filter(|o| *o != id)
            {
                // Mod-click beside the open message picks both, as in any list.
                self.selection.toggle(open);
            }
            self.selection.toggle(id);
        } else {
            self.selection.clear();
            self.open_message(id);
            return;
        }
        self.sync_reading();
        tx.instant();
    }

    /// x: selects or unselects the keyboard's row (the open one, until a selection starts).
    fn select_key(&mut self, tx: &mut Tx) {
        let Some(id) = self.selection.cursor().or(self.open).or(self.list.first().map(|s| s.id)) else { return };
        self.toggle_selected(id);
        tx.scroll_into_view(row_key(id));
        tx.instant();
    }

    /// shift+j, shift+k: selects the next (previous) row too.
    fn extend_selection(&mut self, delta: isize, tx: &mut Tx) {
        let Some(from) = self.selection.cursor().or(self.open).or(self.list.first().map(|s| s.id)) else { return };
        if let Some(to) = self.selection.extend(&self.list, from, delta) {
            tx.scroll_into_view(row_key(to));
            tx.instant();
        }
    }

    /// j, k while rows are selected: the keyboard's row moves; nothing opens.
    fn move_cursor(&mut self, delta: isize, tx: &mut Tx) {
        if let Some(to) = self.selection.move_cursor(&self.list, delta) {
            tx.scroll_into_view(row_key(to));
            tx.instant();
        }
    }

    pub fn picker_state(&mut self, which: Picker) -> &mut ComboboxState {
        match which {
            Picker::Move => &mut self.move_picker,
            Picker::Label => &mut self.label_picker,
            Picker::Snooze => &mut self.snooze_picker,
        }
    }

    /// Opens Move to…, Label… or Snooze… for the targets, its search field ready for typing.
    pub fn open_picker(&mut self, which: Picker) {
        if which == Picker::Snooze {
            return self.open_snooze_picker();
        }
        let ids = self.targets();
        if ids.is_empty() {
            return;
        }
        if which == Picker::Label && !self.all_gmail(&ids) {
            self.toast("Labels are for Gmail accounts. To file mail in a folder, use Move to… (V)", false);
            return;
        }
        let state = self.picker_state(which);
        state.open = true;
        state.search.clear();
        state.search.focused = true;
        state.active = None;
    }

    /// What a picker offers for these messages, and a note on what it leaves out. Move to…:
    /// the inbox, archive, spam and trash, then the account's own folders. Label…: the
    /// account's labels. Messages from several accounts get only what every one of their
    /// accounts has (each role by its own folder there, folders by name), so nothing is sent
    /// to a folder its account lacks. Where every message already is isn't offered.
    ///
    /// Linear in the targets and the mailboxes, and built again only when either changes: the
    /// open picker asks every frame ([`App::picker_cache`]).
    pub fn picker_options(&self, which: Picker, ids: &[MessageId]) -> (Vec<ComboboxOption>, Option<String>) {
        // Times, as of now and what's typed: nothing to keep.
        if which == Picker::Snooze {
            return (self.snooze_options(), None);
        }
        // What the answer depends on: the picker, the targets, and the mailboxes (not their
        // counts). Checking that reads neither the list nor the rows.
        let mailboxes = self.mailboxes_fingerprint();
        let split = which == Picker::Move && self.split_applies();
        if let Some(c) = self.picker_cache.borrow().as_ref()
            && c.which == which
            && c.mailboxes == mailboxes
            && c.split == split
            && c.ids == ids
        {
            return (c.options.clone(), c.note.clone());
        }
        let rows = self.rows_of(ids);
        let places = Self::places(&rows);
        let mut accounts: Vec<&str> = places.iter().map(|(a, _)| *a).collect::<HashSet<_>>().into_iter().collect();
        accounts.sort_unstable();
        let boxes: Vec<&[Mailbox]> = accounts.iter().map(|a| self.mailboxes.get(*a).map_or(&[][..], |m| m.as_slice())).collect();
        let mut options = Self::build_picker(which, &places, &boxes);
        if split {
            // The split inbox's tabs, after the folders' roles: moving there moves nothing on
            // the server.
            let at = options.iter().take_while(|o| o.id.starts_with("role:")).count();
            for (i, c) in Category::ALL.into_iter().enumerate() {
                let mut option = ComboboxOption::new(format!("cat:{}", c.as_str()), c.label());
                option.detail = "Inbox tab".into();
                options.insert(at + i, option);
            }
        }
        let note = (accounts.len() > 1).then(|| format!("Only folders all {} accounts have", accounts.len()));
        *self.picker_cache.borrow_mut() = Some(PickerCache { which, ids: ids.to_vec(), mailboxes, split, options: options.clone(), note: note.clone() });
        (options, note)
    }

    /// Every account's mailboxes as a picker shows them (names, roles, what can be selected,
    /// nesting), hashed: their unread counts change all the time, and don't count.
    fn mailboxes_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        // Each account's hash summed: the map's order doesn't matter.
        self.mailboxes.iter().fold(0u64, |sum, (account, boxes)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            account.hash(&mut h);
            for m in boxes {
                (&m.name, &m.display, m.role, m.selectable, m.depth).hash(&mut h);
            }
            sum.wrapping_add(h.finish())
        })
    }

    /// [`App::picker_options`]'s options for messages in `places`, whose accounts' mailboxes are
    /// `boxes`: the first account's, filtered by name sets of the others'.
    fn build_picker(which: Picker, places: &HashSet<(&str, &str)>, boxes: &[&[Mailbox]]) -> Vec<ComboboxOption> {
        let Some(first) = boxes.first() else { return Vec::new() };
        // Per account: its selectable mailboxes' names, and the roles they have.
        let names: Vec<HashSet<&str>> = boxes.iter().map(|b| b.iter().filter(|m| m.selectable).map(|m| m.name.as_str()).collect()).collect();
        let roles: Vec<HashSet<Role>> = boxes.iter().map(|b| b.iter().filter(|m| m.selectable).map(|m| m.role).collect()).collect();
        // Where the messages are: each place's role, and the names of the mailboxes.
        let role_of: HashMap<(&str, &str), Role> = boxes.iter().flat_map(|b| b.iter()).map(|m| ((m.account.as_str(), m.name.as_str()), m.role)).collect();
        let here_roles: Vec<Role> = places.iter().map(|p| role_of.get(p).copied().unwrap_or(Role::Other)).collect();
        let here_names: HashSet<&str> = places.iter().map(|(_, m)| *m).collect();
        let mut out = Vec::new();
        if which == Picker::Move {
            let wanted_roles: [(&str, &str, &[Role]); 4] =
                [("role:inbox", "Inbox", &[Role::Inbox]), ("role:archive", "Archive", &[Role::Archive, Role::All]), ("role:junk", "Spam", &[Role::Junk]), ("role:trash", "Trash", &[Role::Trash])];
            for (id, label, wanted) in wanted_roles {
                let everywhere = roles.iter().all(|r| wanted.iter().any(|w| r.contains(w)));
                let already = here_roles.iter().all(|r| wanted.contains(r));
                if everywhere && !already {
                    out.push(ComboboxOption::new(id, label));
                }
            }
        }
        for m in first.iter().filter(|m| m.role == Role::Other && m.selectable) {
            let everywhere = names.iter().all(|n| n.contains(m.name.as_str()));
            let already = here_names.len() == 1 && here_names.contains(m.name.as_str());
            if !everywhere || already {
                continue;
            }
            let mut option = ComboboxOption::new(format!("mb:{}", m.name), m.display.clone());
            // A nested folder says where it is: "Clients" in "Work".
            let parent = m.name.rsplit_once(['/', '.']).map(|(p, _)| envoy_core::imap::decode_utf7(p)).unwrap_or_default();
            if m.depth > 0 && parent != m.display {
                option.detail = parent;
            }
            out.push(option);
        }
        out
    }

    /// A picker's choice: moves (undoable, like archive) or labels the targets.
    pub fn pick(&mut self, which: Picker, option: &str, tx: &mut Tx) {
        let ids = self.targets();
        match (which, option) {
            (Picker::Move, "role:inbox") => self.move_to(ids, MoveTarget::Inbox, tx),
            (Picker::Move, "role:archive") => self.archive(ids, tx),
            (Picker::Move, "role:junk") => self.move_to(ids, MoveTarget::Junk, tx),
            (Picker::Move, "role:trash") => self.delete(ids, tx),
            (Picker::Move, o) => {
                if let Some(name) = o.strip_prefix("mb:") {
                    self.move_to(ids, MoveTarget::Mailbox(name.to_owned()), tx);
                } else if let Some(c) = o.strip_prefix("cat:").and_then(Category::parse) {
                    self.move_to_category(ids, c, tx);
                }
            }
            (Picker::Label, o) => {
                if let Some(name) = o.strip_prefix("mb:") {
                    self.label(ids, name.to_owned(), tx);
                }
            }
            (Picker::Snooze, o) => self.pick_snooze(o, tx),
        }
    }

    /// Adds label `mailbox` to messages: a copy there, leaving them where they are. Undo takes
    /// it back while the undo window lasts (the copy hasn't gone to the server yet).
    pub fn label(&mut self, ids: Vec<MessageId>, mailbox: String, tx: &mut Tx) {
        let ids = self.settled(ids);
        if ids.is_empty() {
            return;
        }
        let display = self.mailbox_display(&ids, &mailbox);
        let text = if ids.len() == 1 { format!("Labeled \u{201c}{display}\u{201d}") } else { format!("Labeled {} \u{201c}{display}\u{201d}", ids.len()) };
        let window = self.settings.u32("behavior.undoSeconds");
        let key = (window > 0).then(|| self.next_stamp());
        self.send(Command::Copy { ids, mailbox, thread: self.conversations(), undo: key });
        let Some(key) = key else {
            self.toast(text, false);
            return;
        };
        // Nothing to put back on screen: Undo only calls the copies back.
        let snapshot = MovedAway { key, view: self.view.clone(), query: self.applied_query.clone(), category: self.list_category(), rows: Vec::new(), open: None, compose: None, demo_drafts: Vec::new() };
        self.toast_seq += 1;
        self.toasts.push(Toast { id: self.toast_seq, text, undo: Some(Undo::Move(Box::new(snapshot))), until: self.clock + window as f64, error: false, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        tx.wake_after(std::time::Duration::from_secs_f64(window as f64 + 0.05));
    }

    pub fn toggle_star(&mut self, id: MessageId) {
        if self.settled(vec![id]).is_empty() {
            return;
        }
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
        self.list_changed_here();
    }

    pub fn toggle_read(&mut self, id: MessageId) {
        let unread = self.list.iter().find(|s| s.id == id).map(|s| s.thread_unread).unwrap_or(false);
        self.set_read(vec![id], unread);
    }

    /// Marks rows read (or unread), in one go. Marking the open message unread closes it, as
    /// Gmail does, so it stays unread.
    pub fn set_read(&mut self, ids: Vec<MessageId>, read: bool) {
        let ids = self.settled(ids);
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            self.count_read(*id, read);
        }
        // Read is dealt with: back from a snooze, it's no longer pinned.
        let unpin = if read && self.list.iter().any(|s| ids.contains(&s.id) && s.flags & flags::WOKEN != 0) { flags::WOKEN } else { 0 };
        for s in self.list.iter_mut().filter(|s| ids.contains(&s.id)) {
            s.thread_unread = !read;
            if read { s.flags = (s.flags | flags::SEEN) & !unpin } else { s.flags &= !flags::SEEN }
        }
        let (add, remove) = if read { (flags::SEEN, unpin) } else { (0, flags::SEEN) };
        let open_too = self.open.is_some_and(|o| ids.contains(&o));
        self.send(Command::SetFlags { ids, add, remove, thread: self.conversations() });
        self.list_changed_here();
        if !read && open_too {
            self.close_message();
        }
    }

    /// Stars rows (or unstars them), in one go: starring marks each row's message, unstarring a
    /// conversation clears every star in it, as for one ([`App::toggle_star`]).
    pub fn set_starred(&mut self, ids: Vec<MessageId>, star: bool) {
        let ids = self.settled(ids);
        if ids.is_empty() {
            return;
        }
        for s in self.list.iter_mut().filter(|s| ids.contains(&s.id)) {
            if star { s.flags |= flags::FLAGGED } else { s.flags &= !flags::FLAGGED }
        }
        for m in self.thread.iter_mut().filter(|m| ids.contains(&m.summary.id)) {
            if star { m.summary.flags |= flags::FLAGGED } else { m.summary.flags &= !flags::FLAGGED }
        }
        let (add, remove) = if star { (flags::FLAGGED, 0) } else { (0, flags::FLAGGED) };
        self.send(Command::SetFlags { ids, add, remove, thread: !star && self.conversations() });
        self.list_changed_here();
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
        self.park_compose();
        let account = self.default_account();
        let sig = self.signature_for(&account);
        let mut c = Compose::new(account, self.next_stamp());
        c.to.focused = true;
        c.body = TextEdit::new(sig);
        self.compose = Some(c);
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
        let reply_of = if forward { None } else { Some(s.id) };
        // A forward takes the original's files along, fetched by the engine, pictures shown
        // inside it too (it's written as plain text, so they'd be lost); a reply doesn't.
        let original = s.id;
        let forwarded: Vec<_> = if forward { body.map(|b| b.attachments.iter().chain(&b.inline).cloned().collect()).unwrap_or_default() } else { Vec::new() };
        self.park_compose();
        let mut c = Compose::new(account, self.next_stamp());
        c.show_cc = !cc.is_empty();
        c.to = TextEdit::new(to.clone());
        c.cc = TextEdit::new(cc);
        c.subject = TextEdit::new(subject.clone());
        c.body = TextEdit::new(text);
        if forward || to.is_empty() {
            c.to.focused = true;
        } else {
            c.body.focused = true;
            c.body.anchor = 0;
            c.body.focus = 0;
        }
        c.in_reply_to = irt;
        c.references = refs;
        c.reply_of = reply_of;
        c.title = subject;
        let status = if self.demo { FileStatus::Attached } else { FileStatus::Attaching };
        let files: Vec<(DraftAttachment, usize)> = forwarded
            .iter()
            .map(|a| (DraftAttachment { id: self.next_stamp(), filename: a.filename.clone(), mime: a.mime.clone(), size: a.size, status: status.clone() }, a.index))
            .collect();
        c.files = files.iter().map(|(f, _)| f.clone()).collect();
        let draft_id = c.draft_id;
        self.compose = Some(c);
        if !files.is_empty() && !self.demo {
            // The draft goes in the store first: its files are kept with it.
            self.save_compose(true);
            for (file, index) in &files {
                self.file_ops.push(FileOp::Attach { draft: draft_id, file: file.clone(), source: FileSource::Message { id: original, index: *index } });
            }
            self.send(Command::AttachForwarded { draft: draft_id, message: original, files });
        }
    }

    /// A field of the composer was edited: its draft is saved shortly ([`App::autosave`]).
    pub fn compose_edited(&mut self) {
        if let Some(c) = &mut self.compose {
            c.dirty = true;
            c.edits += 1;
        }
    }

    /// Saves the composer's draft once typing pauses, and every few seconds while it doesn't,
    /// so a crash loses at most that much. Only hands the content to the engine: the frame never
    /// waits on a write. True when the composer changed (it shows "Draft saved").
    fn autosave(&mut self, clock: f64, tx: &mut Tx) -> bool {
        let Some(c) = &mut self.compose else { return false };
        if !c.dirty {
            return false;
        }
        if c.timed != c.edits {
            c.timed = c.edits;
            c.edited_at = clock;
            c.dirty_since.get_or_insert(clock);
        }
        let due = (c.edited_at + SAVE_WHEN_IDLE).min(c.dirty_since.unwrap_or(clock) + SAVE_AT_LEAST_EVERY);
        if clock < due {
            tx.wake_after(std::time::Duration::from_secs_f64(due - clock));
            return false;
        }
        self.save_compose(true);
        true
    }

    /// Saves the composer's content as its draft; `open`: it stays in the composer (and opens
    /// again after a restart). Until the engine confirms it, the save is kept (pending_saves).
    fn save_compose(&mut self, open: bool) {
        let rev = self.next_stamp();
        let Some(c) = &mut self.compose else { return };
        c.dirty = false;
        c.dirty_since = None;
        c.saved = true;
        c.submitted = rev;
        let (id, draft, lifecycle, files_from) = (c.draft_id, c.draft(), c.lifecycle, c.files_from);
        self.submit_save(id, draft, open, rev, lifecycle, files_from);
        if self.view_role() == Some(Role::Drafts) {
            // The list on screen, read again.
            self.request_list_as(false);
        }
    }

    fn submit_save(&mut self, id: i64, draft: Draft, open: bool, rev: i64, lifecycle: i64, files_from: Option<i64>) {
        // A newer save of the same draft supersedes one still waiting.
        let failures = self.pending_saves.iter().find(|p| p.id == id).map_or(0, |p| p.failures);
        self.pending_saves.retain(|p| p.id != id);
        self.pending_saves.push(PendingSave { id, rev, lifecycle, draft: draft.clone(), open, files_from, failures, retry_at: None });
        if self.demo {
            // No engine: kept for the session, for demo mode's Drafts to list.
            self.demo_drafts.keep(id, draft.clone());
        }
        self.send(save_command(id, draft, open, rev, lifecycle, files_from));
    }

    /// The engine wrote save `rev` of draft `id`.
    fn draft_saved(&mut self, id: i64, rev: i64) {
        self.pending_saves.retain(|p| !(p.id == id && p.rev <= rev));
        if let Some(c) = &mut self.compose
            && c.draft_id == id
        {
            c.acked = c.acked.max(rev);
            if rev >= c.submitted {
                c.save_error = None;
                // Its files are its own now.
                c.files_from = None;
            }
        }
    }

    /// The engine couldn't write save `rev` of draft `id`: it isn't saved. The composer says so
    /// (a draft put away gets a toast, once); it's tried again in a few seconds.
    fn draft_save_failed(&mut self, id: i64, rev: i64, error: String) {
        let clock = self.clock;
        let Some(p) = self.pending_saves.iter_mut().find(|p| p.id == id && p.rev == rev) else { return };
        p.failures += 1;
        p.retry_at = Some(clock + SAVE_RETRY);
        let first = p.failures == 1;
        if let Some(c) = self.compose.as_mut().filter(|c| c.draft_id == id) {
            c.save_error = Some(format!("Not saved: {error}"));
        } else if first {
            self.toast(format!("Couldn't save a draft on this computer ({error}). Envoy keeps trying."), true);
        }
    }

    /// The engine refused save `rev` of draft `id`: the draft moved on without it (gone for
    /// good, or sent, since the save was made; say, a too-late Undo put a discarded draft back
    /// in the composer after it was deleted). It isn't saved, and never will be under that id,
    /// so what it holds becomes a new draft: a composer with that draft carries on as the new
    /// one, with whatever it shows now.
    fn draft_save_refused(&mut self, id: i64, rev: i64) {
        let Some(i) = self.pending_saves.iter().position(|p| p.id == id && p.rev == rev) else { return };
        let p = self.pending_saves.remove(i);
        let new_id = self.next_stamp();
        let files_from = Some(p.files_from.unwrap_or(id));
        if let Some(c) = self.compose.as_mut().filter(|c| c.draft_id == id) {
            c.draft_id = new_id;
            c.lifecycle = 0;
            c.files_from = files_from;
            // Its files go with it, those on their way too ([`Command::SaveDraftFrom`]).
            self.save_compose(true);
        } else {
            let rev = self.next_stamp();
            self.submit_save(new_id, p.draft, p.open, rev, 0, files_from);
        }
        self.toast("Saved as a new draft: the one it was saved to had already been sent or deleted", false);
    }

    /// Hands failed saves to the engine again once their wait is over.
    fn retry_saves(&mut self, clock: f64, tx: &mut Tx) {
        let due: Vec<i64> = self.pending_saves.iter().filter(|p| p.retry_at.is_some_and(|t| t <= clock)).map(|p| p.id).collect();
        for id in due {
            let rev = self.next_stamp();
            let Some(p) = self.pending_saves.iter_mut().find(|p| p.id == id) else { continue };
            p.rev = rev;
            p.retry_at = None;
            let (draft, open, lifecycle, files_from) = (p.draft.clone(), p.open, p.lifecycle, p.files_from);
            if let Some(c) = &mut self.compose
                && c.draft_id == id
            {
                c.submitted = rev;
            }
            self.send(save_command(id, draft, open, rev, lifecycle, files_from));
        }
        if let Some(next) = self.pending_saves.iter().filter_map(|p| p.retry_at).reduce(f64::min) {
            tx.wake_after(std::time::Duration::from_secs_f64((next - clock).max(0.05)));
        }
    }

    /// A send couldn't be queued: the message goes back in the composer, from the copy kept
    /// for that, saying why, and is saved again.
    fn send_failed(&mut self, draft_id: Option<i64>, error: String) {
        let Some(id) = draft_id else { return };
        let Some(i) = self.pending_sends.iter().position(|p| p.id == id) else { return };
        let sent = self.pending_sends.remove(i);
        self.show_draft(id, sent.draft, DraftRev { lifecycle: sent.lifecycle, rev: sent.rev }, false, None);
        if let Some(c) = &mut self.compose
            && c.draft_id == id
        {
            c.dirty = true;
            c.edits += 1;
            c.error = Some(error);
        }
    }

    /// Puts the composer away so another can take its place: what's written in it stays in
    /// Drafts.
    fn park_compose(&mut self) {
        let Some(c) = &self.compose else { return };
        if c.dirty || c.saved {
            self.save_compose(false);
            self.toast("Saved to Drafts", false);
        }
        self.compose = None;
    }

    /// The window is closing: everything not confirmed written is written now, before the
    /// process can exit ([`App::unsaved`]). The composer opens again on the next start.
    pub fn on_close(&mut self) {
        if self.demo {
            return;
        }
        let engine = self.engine.take();
        let files = |op: &FileOp| -> Result<(), String> {
            let Some(engine) = &engine else { return Err("the mail database isn't open".into()) };
            match op {
                FileOp::Attach { draft, file, source } => engine.attach_now(*draft, &[(file.clone(), source.clone())]),
                FileOp::Detach { draft, file } => engine.detach_now(*draft, *file),
            }
            .map_err(|e| format!("{e:#}"))
        };
        self.close_with(
            |u| {
                let Some(engine) = &engine else { return Err("the mail database isn't open".into()) };
                match u {
                    Unsaved::Save { id, draft, open, rev, lifecycle, files_from } => engine.save_draft_now(*id, draft, *open, DraftRev { lifecycle: *lifecycle, rev: *rev }, *files_from),
                    Unsaved::Unsent { id, draft, rev, lifecycle } => engine.keep_unsent_now(*id, draft, DraftRev { lifecycle: *lifecycle, rev: *rev }),
                }
                .map_err(|e| format!("{e:#}"))
            },
            |failed| {
                let drafts: Vec<envoy_core::drafts::Recovered> = failed.iter().map(Unsaved::recovered).collect();
                envoy_core::drafts::save_recovery(&envoy_core::drafts::recovery_dir(), &drafts).map_err(|e| format!("{e:#}"))
            },
        );
        // Then files added and taken off that the engine may not have got to: the text is in, so
        // their draft is too (or the one that replaced it, which close_with points them at).
        self.flush_files(&files);
        self.file_ops.clear();
        self.engine = engine;
    }

    /// Makes again, through `apply`, every change to a draft's files handed to the engine and
    /// not known to be done ([`App::file_ops`]): the engine, or the process, may stop before it
    /// gets to them. Each is harmless made twice: an attach lists a file being attached (the
    /// next start reads it in) unless its draft has it, or had it; taking off one that's off
    /// changes nothing.
    pub fn flush_files(&self, mut apply: impl FnMut(&FileOp) -> Result<(), String>) {
        for op in &self.file_ops {
            if let Err(e) = apply(op) {
                eprintln!("couldn't keep a change to a draft's files ({e}): {op:?}");
            }
        }
    }

    /// [`App::on_close`]'s work: everything unsaved goes through `write` (false: the store
    /// refused it, its draft having moved on, so it's written again as a new draft), and what
    /// fails to `recover` (the recovery file, imported on the next start). A snapshot is
    /// dropped only once one of them holds it; if neither can, its text goes to the log, the
    /// last place left.
    fn close_with(&mut self, mut write: impl FnMut(&Unsaved) -> Result<bool, String>, recover: impl FnOnce(Vec<Unsaved>) -> Result<(), String>) {
        let mut failed = Vec::new();
        // Drafts written again as new ones: old id, and the new one its file changes went to.
        let mut replaced: Vec<(i64, i64)> = Vec::new();
        for u in self.unsaved() {
            let old = u.id();
            let u = match write(&u) {
                Ok(true) => continue,
                Ok(false) => {
                    let (id, rev) = (self.next_stamp(), self.next_stamp());
                    u.as_new_draft(id, rev)
                }
                Err(e) => {
                    eprintln!("couldn't save a draft ({e}): keeping it in the recovery file");
                    failed.push(u);
                    continue;
                }
            };
            match write(&u) {
                // Its files' changes not yet made go to the new draft (the latest, if several).
                Ok(true) => {
                    let new = u.id();
                    let before = replaced.iter().rev().find(|r| r.0 == old).map_or(old, |r| r.1);
                    replaced.push((old, new));
                    for op in &mut self.file_ops {
                        let (FileOp::Attach { draft, .. } | FileOp::Detach { draft, .. }) = op;
                        if *draft == before {
                            *draft = new;
                        }
                    }
                }
                Ok(false) => failed.push(u),
                Err(e) => {
                    eprintln!("couldn't save a draft ({e}): keeping it in the recovery file");
                    failed.push(u);
                }
            }
        }
        if failed.is_empty() {
            return;
        }
        let text: Vec<String> = failed.iter().map(|u| format!("{u:#?}")).collect();
        if let Err(e) = recover(failed) {
            eprintln!("ENVOY COULDN'T SAVE {} DRAFT(S) ANYWHERE ({e}). THEIR TEXT, TO COPY FROM HERE:", text.len());
            for t in text {
                eprintln!("{t}");
            }
        }
    }

    /// What isn't confirmed written, oldest first, taken for writing on the spot: saves handed
    /// to the engine (each under its stamp, so one the engine already applied, or overtook,
    /// changes nothing), the composer's unsaved edits, and messages sent but not confirmed
    /// queued.
    pub fn unsaved(&mut self) -> Vec<Unsaved> {
        let mut saves = std::mem::take(&mut self.pending_saves);
        saves.sort_by_key(|p| p.rev);
        let mut out: Vec<Unsaved> =
            saves.into_iter().map(|p| Unsaved::Save { id: p.id, draft: p.draft, open: p.open, rev: p.rev, lifecycle: p.lifecycle, files_from: p.files_from }).collect();
        let rev = self.next_stamp();
        if let Some(c) = &mut self.compose
            && c.dirty
        {
            c.dirty = false;
            out.push(Unsaved::Save { id: c.draft_id, draft: c.draft(), open: true, rev, lifecycle: c.lifecycle, files_from: c.files_from });
        }
        out.extend(std::mem::take(&mut self.pending_sends).into_iter().map(|p| Unsaved::Unsent { id: p.id, draft: p.draft, rev: p.rev, lifecycle: p.lifecycle }));
        out
    }

    /// Discards the composer and its draft. What was written stays in the toast's Undo, which
    /// brings the composer back as it was; the draft (here and on the server) goes when the
    /// toast does. An untouched composer just closes.
    pub fn discard_compose(&mut self, tx: &mut Tx) {
        let Some(c) = self.compose.take() else { return };
        // Discarded: a save still waiting mustn't bring it back when the window closes.
        self.pending_saves.retain(|p| p.id != c.draft_id);
        if !c.dirty && !c.saved {
            return;
        }
        let window = self.settings.u32("behavior.undoSeconds");
        let key = (window > 0).then(|| self.next_stamp());
        if c.saved {
            self.send(Command::DiscardDraft { id: c.draft_id, undo: key });
            self.demo_draft_gone(c.draft_id);
        }
        let Some(key) = key else {
            self.toast("Draft discarded", false);
            return;
        };
        self.toast_seq += 1;
        let undo = Undo::Discard(Box::new(Discarded { key: c.saved.then_some(key), compose: c }));
        self.toasts.push(Toast { id: self.toast_seq, text: "Draft discarded".into(), undo: Some(undo), until: self.clock + window as f64, error: false, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        tx.wake_after(std::time::Duration::from_secs_f64(window as f64 + 0.05));
    }

    /// A draft sent or discarded in demo mode leaves its Drafts (Undo saves it again).
    fn demo_draft_gone(&mut self, id: i64) {
        if !self.demo {
            return;
        }
        self.demo_drafts.forget(id);
        if self.view_role() == Some(Role::Drafts) {
            self.request_list_as(false);
        }
    }

    /// Undo for [`App::discard_compose`]: the composer comes back as it was, and is saved again
    /// (which marks its draft as open in the composer).
    fn undo_discard(&mut self, d: Discarded) {
        if let Some(undo) = d.key {
            self.send(Command::UndoMove { undo });
        }
        self.reopen(d.compose);
    }

    /// Puts a discarded composer back as it was, saving it again (which marks its draft as open
    /// in the composer).
    fn reopen(&mut self, mut c: Compose) {
        self.park_compose();
        c.minimized = false;
        c.dirty = true;
        c.edits += 1;
        self.compose = Some(c);
    }

    /// Opens a Drafts row in the composer. The engine answers with the draft
    /// ([`Event::DraftOpened`]).
    fn open_draft(&mut self, id: MessageId) {
        if self.demo {
            if let Some((id, draft)) = self.demo_drafts.of_row(id).or_else(|| crate::demo::draft_for(id).map(|d| (id, d))) {
                self.show_draft(id, draft, DraftRev::default(), false, None);
            }
            return;
        }
        self.send(Command::OpenDraft { id });
    }

    /// Shows a draft in the composer: opened from Drafts, back from an undone send, or
    /// (`restored`, minimized) left there when the app last closed. The composer it replaces is
    /// put away in Drafts, unless it's this same draft (it may have newer edits), or this is a
    /// restore and the user already started another. With [`DraftNotice::Copy`] it's an unsaved
    /// copy of a server draft that stays as it is, and gets an id of its own here. `at`: the
    /// draft's lifecycle, for its saves to carry, and its newest save's stamp, which the
    /// composer's stamps go on from (whatever the clock says).
    pub fn show_draft(&mut self, id: i64, draft: Draft, at: DraftRev, restored: bool, notice: Option<DraftNotice>) {
        self.last_stamp = self.last_stamp.max(at.rev);
        let copy = matches!(notice, Some(DraftNotice::Copy(_)));
        let (id, lifecycle) = if copy { (self.next_stamp(), 0) } else { (id, at.lifecycle) };
        if let Some(c) = &mut self.compose {
            if c.draft_id == id {
                c.minimized &= restored;
                c.lifecycle = lifecycle;
                return;
            }
            if restored {
                return;
            }
        }
        self.park_compose();
        let mut c = Compose::new(draft.account.clone(), id);
        c.lifecycle = lifecycle;
        c.title = "Draft".into();
        c.show_cc = !draft.cc.trim().is_empty() || !draft.bcc.trim().is_empty();
        c.to = TextEdit::new(draft.to);
        c.cc = TextEdit::new(draft.cc);
        c.bcc = TextEdit::new(draft.bcc);
        c.subject = TextEdit::new(draft.subject);
        c.body = TextEdit::new(draft.body);
        c.in_reply_to = draft.in_reply_to;
        c.references = draft.references;
        c.reply_of = draft.reply_of;
        // New files' ids come after every one it has (stamps go on from the largest).
        self.last_stamp = self.last_stamp.max(draft.attachments.iter().map(|f| f.id).max().unwrap_or(0));
        c.files = draft.attachments;
        c.notice = notice.map(|n| match n {
            DraftNotice::PlainText => "This draft has formatting Envoy doesn't write: saving an edit keeps its text and attachments, not the formatting.".to_owned(),
            DraftNotice::Copy(why) => format!("Envoy can't edit this draft in place ({why}), so it stays in Drafts as it is. This is a copy, without its attachments: what you write is saved as a new draft."),
            DraftNotice::NotSent(why) => why,
        });
        c.saved = !copy;
        c.minimized = restored;
        if !restored {
            if c.to.text.trim().is_empty() {
                c.to.focused = true;
            } else {
                // Where a reply starts too: above any quote and signature.
                c.body.focused = true;
                c.body.anchor = 0;
                c.body.focus = 0;
            }
        }
        self.compose = Some(c);
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
        // Never sent without a file the composer shows, nor bounced for its size.
        if let Some(f) = c.files.iter().find(|f| f.status == FileStatus::Attaching) {
            c.error = Some(format!("Still attaching {}…", f.filename));
            return;
        }
        if let Some(f) = c.files.iter().find(|f| matches!(f.status, FileStatus::Failed(_))) {
            c.error = Some(format!("{} isn't attached: remove it, or attach it again", f.filename));
            return;
        }
        let limit = attach::Limit::new(self.accounts.iter().find(|a| a.id == c.account), self.send_limits.get(&c.account).copied());
        let (sizes, text) = c.sizes();
        if let Some(why) = limit.over(&sizes, text) {
            c.error = Some(why);
            return;
        }
        let (draft, draft_id, lifecycle) = (c.draft(), c.draft_id, c.lifecycle);
        self.compose = None;
        if self.demo {
            self.demo_draft_gone(draft_id);
            self.toast("Sent (demo mode — nothing left your computer)", false);
            return;
        }
        // Kept until the engine confirms it queued the message (Event::Sent), or gives it back
        // (Event::SendFailed), and written as a draft if the window closes first.
        let rev = self.next_stamp();
        self.pending_sends.push(PendingSend { id: draft_id, draft: draft.clone(), rev, lifecycle });
        // What was sent supersedes saves still waiting (a failed one's retry would write older
        // text over the draft kept for Undo send).
        self.pending_saves.retain(|p| p.id != draft_id);
        // The draft goes with it; undoing the send brings it back (Event::DraftOpened).
        self.send(Command::Send { draft, draft_id: Some(draft_id) });
        if self.view_role() == Some(Role::Drafts) {
            // The list on screen, read again.
            self.request_list_as(false);
        }
    }

    // ── attachments ──

    /// Opens the system's file picker for the composer (several files at once); what's picked
    /// is attached.
    pub fn attach_dialog(&mut self, tx: &mut Tx) {
        let Some(c) = &mut self.compose else { return };
        c.minimized = false;
        let options = rune::FileDialogOptions { title: "Attach files".into(), ..Default::default() };
        tx.open_files(options, |s: &mut App, picked, _| match picked {
            Ok(paths) => s.attach_paths(paths),
            Err(e) => s.toast(format!("Couldn't open the file picker: {e}"), true),
        });
    }

    /// How big a message from the composer's account may be.
    pub fn compose_limit(&self) -> attach::Limit {
        let account = self.compose.as_ref().map(|c| c.account.as_str()).unwrap_or_default();
        attach::Limit::new(self.accounts.iter().find(|a| a.id == account), self.send_limits.get(account).copied())
    }

    /// Attaches files from this computer (picked, or dropped on the composer). They show at
    /// once, as being attached, and are listed with the draft in the store as they are, while
    /// the engine reads them in off this thread; the store gets each whether the composer is
    /// still there or not ([`Command::AttachFiles`]). Each id is a stamp: never one a file had.
    pub fn attach_paths(&mut self, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() || self.compose.is_none() {
            return;
        }
        let ids: Vec<i64> = paths.iter().map(|_| self.next_stamp()).collect();
        let max = self.compose_limit().max_file();
        let Some(c) = &mut self.compose else { return };
        c.minimized = false;
        c.drop_hover = false;
        c.error = None;
        let mut files = Vec::new();
        let draft = c.draft_id;
        for (id, path) in ids.into_iter().zip(paths) {
            let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            let file = DraftAttachment { id, mime: attach::type_for(&filename).to_owned(), filename, size: 0, status: FileStatus::Attaching };
            if !self.demo {
                self.file_ops.push(FileOp::Attach { draft, file: file.clone(), source: FileSource::Path(path.clone()) });
            }
            c.files.push(file);
            files.push((id, path));
        }
        if self.demo {
            let at = self.clock + 0.8;
            for (file, path) in files {
                // Off the UI thread, even in demo mode: a disk can take its time.
                let found = std::sync::Arc::new(std::sync::Mutex::new(None));
                let into = found.clone();
                std::thread::spawn(move || {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let r = match std::fs::metadata(&path) {
                        Ok(m) if m.is_dir() => Err(format!("{name} is a folder: attach the files in it")),
                        Ok(m) if m.len() as usize > max => Err(attach::too_big(&name, m.len() as usize, max)),
                        Ok(m) => Ok(m.len()),
                        Err(e) => Err(format!("Couldn't read {name}: {e}")),
                    };
                    if let Ok(mut f) = into.lock() {
                        *f = Some(r);
                    }
                });
                self.demo_reads.push(DemoRead { at, draft, file, found });
            }
            // Saved with its files, as the engine's path saves a draft before attaching: put
            // away before they're read, it's still in Drafts, and they land there.
            self.compose_edited();
            return;
        }
        // The draft goes in the store first: a file is kept with its draft, never without one.
        if !c.saved {
            self.save_compose(true);
        }
        self.send(Command::AttachFiles { draft, files });
    }

    /// Takes a file off the composer, and off the draft in the store.
    pub fn remove_file(&mut self, id: i64) {
        let Some(c) = &mut self.compose else { return };
        let Some(i) = c.files.iter().position(|f| f.id == id) else { return };
        c.files.remove(i);
        c.error = None;
        let draft = c.draft_id;
        if !self.demo {
            self.file_ops.retain(|op| !matches!(op, FileOp::Attach { file, .. } if file.id == id));
            // Made again on close, harmlessly: no answer says it's done, so it's never dropped.
            self.file_ops.push(FileOp::Detach { draft, file: id });
            self.send(Command::DetachFile { draft, file: id });
        }
    }

    /// The composer (or one closed, waiting in an Undo) holding draft `draft`.
    fn compose_for(&mut self, draft: i64) -> Option<&mut Compose> {
        if self.compose.as_ref().is_some_and(|c| c.draft_id == draft) {
            return self.compose.as_mut();
        }
        self.toasts.iter_mut().find_map(|t| match &mut t.undo {
            Some(Undo::Discard(d)) if d.compose.draft_id == draft => Some(&mut d.compose),
            Some(Undo::Move(m)) => m.compose.as_deref_mut().filter(|c| c.draft_id == draft),
            _ => None,
        })
    }

    /// A file of draft `draft` is in: its chip says so. (The store has it whether or not a
    /// composer shows the draft; one taken off here stays off.)
    fn file_attached(&mut self, draft: i64, file: DraftAttachment) {
        self.file_ops.retain(|op| !matches!(op, FileOp::Attach { file: f, .. } if f.id == file.id));
        if let Some(f) = self.compose_for(draft).and_then(|c| c.file_mut(file.id)) {
            *f = file;
        }
    }

    fn attach_failed(&mut self, draft: i64, file: i64, error: String) {
        self.file_ops.retain(|op| !matches!(op, FileOp::Attach { file: f, .. } if f.id == file));
        if let Some(f) = self.compose_for(draft).and_then(|c| c.file_mut(file)).filter(|f| f.status == FileStatus::Attaching) {
            f.status = FileStatus::Failed(error);
        }
    }

    /// Draft `draft`'s files as the store has them ([`Event::DraftFiles`]): the composer shows
    /// them, and keeps files it's attaching that the engine hasn't listed yet, and leaves off
    /// files it's taking off.
    fn draft_files(&mut self, draft: i64, files: Vec<DraftAttachment>) {
        let listed: HashSet<i64> = files.iter().map(|f| f.id).collect();
        self.file_ops.retain(|op| !matches!(op, FileOp::Attach { file, .. } if listed.contains(&file.id)));
        let adding: HashSet<i64> = self.file_ops.iter().filter_map(|op| if let FileOp::Attach { file, .. } = op { Some(file.id) } else { None }).collect();
        let removing: HashSet<i64> = self.file_ops.iter().filter_map(|op| if let FileOp::Detach { file, .. } = op { Some(*file) } else { None }).collect();
        let Some(c) = self.compose_for(draft) else { return };
        let mut shown: Vec<DraftAttachment> = files.into_iter().filter(|f| !removing.contains(&f.id)).collect();
        shown.extend(c.files.iter().filter(|f| adding.contains(&f.id) && !listed.contains(&f.id)).cloned());
        shown.sort_by_key(|f| f.id);
        c.files = shown;
    }

    /// Demo mode's pretend reads that are done: the file's size as its thread found it,
    /// nothing stored.
    fn finish_demo_reads(&mut self, clock: f64, tx: &mut Tx) -> bool {
        let (done, waiting): (Vec<DemoRead>, Vec<DemoRead>) =
            std::mem::take(&mut self.demo_reads).into_iter().partition(|r| r.at <= clock && r.found.lock().is_ok_and(|f| f.is_some()));
        self.demo_reads = waiting;
        if let Some(next) = self.demo_reads.iter().map(|r| r.at).reduce(f64::min) {
            tx.wake_after(std::time::Duration::from_secs_f64((next - clock).max(0.02)));
        }
        let mut changed = false;
        for r in done {
            let Some(found) = r.found.lock().ok().and_then(|mut f| f.take()) else { continue };
            let land = |f: &mut DraftAttachment| {
                if f.status != FileStatus::Attaching {
                    return false;
                }
                match &found {
                    Ok(size) => (f.size, f.status) = (*size as usize, FileStatus::Attached),
                    Err(why) => f.status = FileStatus::Failed(why.clone()),
                }
                true
            };
            if let Some(f) = self.compose_for(r.draft).and_then(|c| c.file_mut(r.file)) {
                changed |= land(f);
            }
            // The draft as demo mode keeps it, whether a composer shows it or it was put away.
            if let Some(f) = self.demo_drafts.file_mut(r.draft, r.file) {
                changed |= land(f);
            }
        }
        changed
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
        // Select all is the field's while typing (its text), and only for the mail list.
        if action == Action::SelectAll && (typing || self.screen != Screen::Mail) {
            return false;
        }
        // Alt+digit types a character in a field (on a Mac): not a correction then.
        if typing && matches!(action, Action::MoveImportant | Action::MoveOther | Action::MoveNewsletters | Action::MoveNotifications) {
            return false;
        }
        if self.compose.is_some() {
            match action {
                Action::Send => {
                    self.send_compose();
                    return true;
                }
                Action::Attach => {
                    self.attach_dialog(tx);
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
            Action::Archive => self.archive(self.targets(), tx),
            Action::Delete => self.delete(self.targets(), tx),
            Action::Spam => self.spam(self.targets(), tx),
            Action::Undo => {
                if !self.undo_last(tx) {
                    return false;
                }
            }
            Action::Star if !self.selection.is_empty() => {
                let ids = self.targets();
                // Any not starred: star them all; all starred: unstar them.
                let star = self.list.iter().filter(|s| ids.contains(&s.id)).any(|s| !s.flagged());
                self.set_starred(ids, star);
            }
            Action::Star => {
                if let Some(id) = self.thread.last().map(|m| m.summary.id).or(self.open) {
                    self.toggle_star(id);
                }
            }
            Action::ToggleRead if !self.selection.is_empty() => {
                let ids = self.targets();
                // Any unread: mark them all read; all read: unread.
                let read = self.list.iter().filter(|s| ids.contains(&s.id)).any(|s| s.thread_unread);
                self.set_read(ids, read);
            }
            Action::ToggleRead => {
                if let Some(id) = self.open {
                    self.toggle_read(id);
                }
            }
            Action::Next if !self.selection.is_empty() => self.move_cursor(1, tx),
            Action::Previous if !self.selection.is_empty() => self.move_cursor(-1, tx),
            Action::Next => self.step(1, tx),
            Action::Previous => self.step(-1, tx),
            Action::Select => self.select_key(tx),
            Action::SelectNext => self.extend_selection(1, tx),
            Action::SelectPrevious => self.extend_selection(-1, tx),
            Action::SelectAll => {
                self.selection.select_all(&self.list);
                tx.instant();
            }
            Action::MoveTo => self.open_picker(Picker::Move),
            Action::Label => self.open_picker(Picker::Label),
            Action::Snooze => self.open_picker(Picker::Snooze),
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
            Action::Send | Action::Attach => return false,
            Action::Close => {
                if self.confirm_delete.is_some() {
                    self.confirm_delete = None;
                } else if self.screen != Screen::Mail && !self.accounts.is_empty() {
                    self.screen = Screen::Mail;
                } else if !self.selection.is_empty() {
                    self.clear_selection();
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
            Action::ShowImportant => return self.screen == Screen::Mail && self.show_category(Category::Important),
            Action::ShowOther => return self.screen == Screen::Mail && self.show_category(Category::Other),
            Action::ShowNewsletters => return self.screen == Screen::Mail && self.show_category(Category::Newsletters),
            Action::ShowNotifications => return self.screen == Screen::Mail && self.show_category(Category::Notifications),
            Action::MoveImportant => return self.move_to_category(self.targets(), Category::Important, tx),
            Action::MoveOther => return self.move_to_category(self.targets(), Category::Other, tx),
            Action::MoveNewsletters => return self.move_to_category(self.targets(), Category::Newsletters, tx),
            Action::MoveNotifications => return self.move_to_category(self.targets(), Category::Notifications, tx),
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
    fn remote_images_wait_for_the_reader_to_allow_them() {
        // Remote content is blocked by default (North Star); embedded pictures always show.
        let mut app = App::new(true); // demo: settings.json is never written
        crate::demo::populate(&mut app);
        let id = app.list[0].id;
        let mut thread = crate::demo::thread_for(id);
        let m = thread.last_mut().unwrap();
        m.summary.from.email = "News@Shop.example".into();
        m.body.as_mut().unwrap().doc = Some(std::sync::Arc::new(envoy_core::html::parse(
            r#"<img src="https://img.example/a.png" width="80" height="40"><img src="cid:logo@shop" width="80" height="40">
               <div style="background:url(https://img.example/hero.jpg)">Hi</div><td width=1 height=1 background="https://t.example/open"></td>"#,
        )));
        let mid = m.summary.id;
        app.thread = thread;
        let urls = |app: &App| app.wanted_images().into_iter().map(|(u, _)| u).collect::<Vec<_>>();
        let all = ["https://img.example/a.png", "cid:logo@shop", "https://img.example/hero.jpg"];
        assert_eq!(urls(&app), ["cid:logo@shop"], "by default only the embedded picture");
        // "Load images": this message's, this session.
        app.images_shown.insert(mid);
        assert_eq!(urls(&app), all);
        app.images_shown.clear();
        // "Always load from <sender>": saved in reading.imagesFrom. (Nothing open meanwhile:
        // a changed image setting fetches at once, and demo mode fetches inline.)
        let thread = std::mem::take(&mut app.thread);
        app.allow_sender("News@Shop.example");
        app.thread = thread;
        assert_eq!(app.settings.list("reading.imagesFrom"), ["news@shop.example"]);
        assert_eq!(urls(&app), all);
        // "Never" means never; "Always", always.
        let _ = app.settings.set("reading.loadImages", serde_json::json!("never"));
        app.images_shown.insert(mid);
        assert_eq!(urls(&app), ["cid:logo@shop"]);
        let _ = app.settings.set("reading.loadImages", serde_json::json!("always"));
        let _ = app.settings.set("reading.imagesFrom", serde_json::json!([]));
        app.images_shown.clear();
        assert_eq!(urls(&app), all);
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
    fn auto_mark_read_waits_for_the_message_to_load() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // threads come from the engine, later
        let _ = app.settings.set("reading.markAsRead", serde_json::json!("immediately"));
        let id = app.list.iter().find(|s| s.thread_unread).unwrap().id;
        let unread = |app: &App| app.list.iter().find(|s| s.id == id).unwrap().thread_unread;
        let mut tx = Tx::default();
        // Opened (say, by a late Undo) but the engine finds nothing to show: not marked read.
        app.open_message(id);
        assert!(unread(&app), "nothing is marked read before it loads");
        let req = app.thread_req;
        app.apply(Event::Thread { req, id, messages: vec![] }, &mut tx);
        assert_eq!(app.open, None);
        assert!(unread(&app), "a message that never showed stays unread");
        // Opened and loaded: marked read as usual, once.
        app.open_message(id);
        let req = app.thread_req;
        app.apply(Event::Thread { req, id, messages: crate::demo::thread_for(id) }, &mut tx);
        assert!(!unread(&app), "marked read when it shows");
    }

    #[test]
    fn a_neighbour_read_ahead_opens_at_once() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // threads come from the engine
        let mut tx = Tx::default();
        let (first, second) = (app.list[0].id, app.list[1].id);
        app.open_message(first);
        let req = app.thread_req;
        app.apply(Event::Thread { req, id: first, messages: crate::demo::thread_for(first) }, &mut tx);
        // The engine reads the next row ahead, and j shows it at once, before the Open returns.
        app.apply(Event::Prefetched { id: second, messages: crate::demo::thread_for(second) }, &mut tx);
        app.step(1, &mut tx);
        assert_eq!((app.open, app.thread_for), (Some(second), Some(second)));
        assert_eq!(app.thread, crate::demo::thread_for(second));
        // Only the open row's neighbours are kept.
        let far = app.list[app.list.len() - 1].id;
        app.apply(Event::Prefetched { id: far, messages: crate::demo::thread_for(far) }, &mut tx);
        assert!(!app.prefetched.contains_key(&far));
    }

    /// A list of `n` rows, newest first, made from one of the demo's.
    fn long_list(app: &App, n: i64) -> Vec<Summary> {
        let row = app.list[0].clone();
        (1..=n).map(|id| Summary { id, date: 1_000_000 - id, thread: format!("t{id}"), ..row.clone() }).collect()
    }

    fn last_list_request(app: &App) -> String {
        app.sent.borrow().iter().rev().find(|c| c.starts_with("List")).cloned().unwrap_or_default()
    }

    #[test]
    fn saving_in_the_drafts_view_refreshes_its_list() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.go(View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() });
        app.demo = false;
        typed_and_saved(&mut app, "Dear Ana,");
        // The list on screen read again: whole, its changes animating, the open row kept.
        assert!(last_list_request(&app).contains("stream: false"), "{}", last_list_request(&app));
    }

    #[test]
    fn a_refresh_keeps_the_whole_list_and_the_open_row() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // lists come from the engine
        let mut tx = Tx::default();
        app.list = long_list(&app, 3000);
        app.open = Some(500);
        // New mail or a sync: the list on screen is read again, whole, in one go.
        app.reload();
        assert!(last_list_request(&app).contains("stream: false"), "{}", last_list_request(&app));
        // Even a first page sent ahead for it wouldn't cut the list down under the reader.
        let req = app.list_req;
        let page = app.list[..100].to_vec();
        app.apply(Event::List { req, items: page, more: true }, &mut tx);
        assert_eq!(app.list.len(), 3000, "the list keeps its rows");
        app.step(1, &mut tx);
        assert_eq!(app.open, Some(501), "j moves on from the open row, not back to the top");
        // The whole list arrives with a new message on top: the open row is still there.
        let mut fresh = long_list(&app, 3000);
        fresh.insert(0, Summary { id: 9999, date: 2_000_000, thread: "new".into(), ..fresh[0].clone() });
        app.apply(Event::List { req, items: fresh, more: false }, &mut tx);
        assert_eq!(app.list.len(), 3001);
        assert_eq!(app.list.iter().position(|m| Some(m.id) == app.open), Some(501));
    }

    #[test]
    fn a_new_list_shows_its_first_page_and_j_waits_for_the_rest() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let mut tx = Tx::default();
        app.list = long_list(&app, 3000);
        app.open = Some(500);
        // A search: its first page is sent ahead and shown at once.
        app.applied_query = "x".into();
        app.request_list();
        assert!(last_list_request(&app).contains("stream: true"), "{}", last_list_request(&app));
        let req = app.list_req;
        let all = long_list(&app, 2000);
        app.apply(Event::List { req, items: all[..100].to_vec(), more: true }, &mut tx);
        assert_eq!((app.list.len(), app.list_complete), (100, false));
        // The open row isn't in the first page yet: j waits for the rest instead of jumping to
        // the top, then moves on from it.
        app.step(1, &mut tx);
        assert_eq!(app.open, Some(500));
        app.apply(Event::List { req, items: all.clone(), more: false }, &mut tx);
        assert_eq!(app.open, Some(501));
        // A j waiting like that is dropped if the message is closed meanwhile.
        app.request_list();
        let req = app.list_req;
        app.apply(Event::List { req, items: all[..100].to_vec(), more: true }, &mut tx);
        app.step(1, &mut tx);
        app.close_message();
        app.apply(Event::List { req, items: all, more: false }, &mut tx);
        assert_eq!(app.open, None, "nothing opens by itself");
    }

    #[test]
    fn undo_puts_same_second_rows_back_in_the_stores_order() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let mut tx = Tx::default();
        // Same second: the store orders by id, newest first.
        let row = app.list[0].clone();
        app.list = (1..=3).rev().map(|id| Summary { id, date: 100, thread: format!("t{id}"), ..row.clone() }).collect();
        app.open = Some(3);
        app.archive(vec![3], &mut tx);
        assert!(app.undo_last(&mut tx));
        assert_eq!(app.list.iter().map(|m| m.id).collect::<Vec<_>>(), vec![3, 2, 1], "back where the store puts it");
        assert_eq!(app.open, Some(3));
        app.step(1, &mut tx);
        assert_eq!(app.open, Some(2), "j goes on to the next row, before any refresh");
    }

    #[test]
    fn j_at_the_end_of_a_first_page_waits_for_the_rest() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let mut tx = Tx::default();
        let all = long_list(&app, 500);
        app.list = all.clone();
        app.open = Some(100);
        app.request_list();
        let req = app.list_req;
        // The open row is the first page's last: j has nowhere to go until the rest comes.
        app.apply(Event::List { req, items: all[..100].to_vec(), more: true }, &mut tx);
        app.step(1, &mut tx);
        assert_eq!(app.open, Some(100), "j isn't swallowed by the page's end");
        app.step(1, &mut tx);
        app.apply(Event::List { req, items: all, more: false }, &mut tx);
        assert_eq!(app.open, Some(102), "both presses count, from the open row");
        // k inside the page needs no waiting.
        app.request_list();
        let req = app.list_req;
        app.apply(Event::List { req, items: long_list(&app, 500)[..200].to_vec(), more: true }, &mut tx);
        app.step(-1, &mut tx);
        assert_eq!(app.open, Some(101));
    }

    fn move_key(app: &App) -> i64 {
        match &app.toasts.last().expect("a toast").undo {
            Some(Undo::Move(m)) => m.key,
            _ => panic!("no Undo on the toast"),
        }
    }

    #[test]
    fn undo_is_withdrawn_when_the_move_had_nothing_to_do() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let first = app.list[0].id;
        app.archive(vec![first], &mut tx);
        let key = move_key(&app);
        // The engine found the message already moving (or already archived): nothing held.
        app.apply(Event::Held { undo: key, ops: 0 }, &mut tx);
        assert!(app.toasts.last().unwrap().undo.is_none(), "no Undo button for a move that did nothing");
        assert!(!app.undo_last(&mut tx), "and z has nothing to undo");
        // Nothing moved (say, it was already in All Mail), so the row the UI took away comes
        // back: the list is reloaded (pump does this when apply asks for it).
        assert!(!app.list.iter().any(|s| s.id == first));
        let reload = app.apply(Event::Held { undo: key, ops: 0 }, &mut tx);
        assert!(reload, "a zero-op move reloads the list");
        app.reload();
        assert!(app.list.iter().any(|s| s.id == first), "the row is back");
        // A move that did hold its ops keeps its Undo.
        let next = app.list[0].id;
        app.archive(vec![next], &mut tx);
        let key = move_key(&app);
        app.apply(Event::Held { undo: key, ops: 1 }, &mut tx);
        assert_eq!(move_key(&app), key);
    }

    fn type_into_body(app: &mut App, text: &str) {
        app.compose.as_mut().unwrap().body.text.insert_str(0, text);
        app.compose_edited();
    }

    #[test]
    fn a_composers_title_is_its_subject_once_one_is_typed() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let title = |app: &App| app.compose.as_ref().unwrap().shown_title().to_owned();
        app.compose_new();
        assert_eq!(title(&app), "New message");
        app.compose.as_mut().unwrap().minimized = true;
        app.compose.as_mut().unwrap().subject.text = "Lunch on Friday".into();
        app.compose_edited();
        assert_eq!(title(&app), "Lunch on Friday", "minimized, it says which message it is");
        app.compose.as_mut().unwrap().subject.text = "  ".into();
        assert_eq!(title(&app), "New message", "and what it was once the subject is cleared");
        // A draft opened from Drafts: its subject, as it's edited.
        let draft = Draft { account: "demo".into(), subject: "Plans".into(), ..Draft::default() };
        app.apply(Event::DraftOpened { id: 9, draft, at: DraftRev::default(), restored: false, notice: None }, &mut Tx::default());
        assert_eq!(title(&app), "Plans");
        app.compose.as_mut().unwrap().subject.text = "Plans, moved".into();
        assert_eq!(title(&app), "Plans, moved");
        app.compose.as_mut().unwrap().subject.text.clear();
        assert_eq!(title(&app), "Draft", "not the subject it had");
    }

    #[test]
    fn the_draft_saves_when_typing_pauses_and_while_it_doesnt() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        app.compose_new();
        let dirty = |app: &App| app.compose.as_ref().unwrap().dirty;
        app.tick(10.0, &mut tx);
        assert!(!dirty(&app) && !app.compose.as_ref().unwrap().saved, "nothing typed, nothing saved");
        // Typing steadily: saved within SAVE_AT_LEAST_EVERY even though it never pauses.
        let mut t = 10.0;
        while dirty(&app) || t == 10.0 {
            type_into_body(&mut app, "a");
            t += 0.25;
            app.tick(t, &mut tx);
            assert!(t < 10.0 + SAVE_AT_LEAST_EVERY + 0.3, "saved while typing continues");
        }
        assert!(t >= 10.0 + SAVE_AT_LEAST_EVERY, "but not on every keystroke (saved at {t})");
        assert!(app.compose.as_ref().unwrap().saved);
        // One more keystroke: saved once typing pauses.
        type_into_body(&mut app, "b");
        app.tick(t + 0.1, &mut tx);
        assert!(dirty(&app));
        app.tick(t + 0.1 + SAVE_WHEN_IDLE / 2.0, &mut tx);
        assert!(dirty(&app), "still typing, as far as it knows");
        app.tick(t + 0.1 + SAVE_WHEN_IDLE, &mut tx);
        assert!(!dirty(&app), "saved after the pause");
    }

    /// Types into a new composer and lets the autosave hand it to the engine.
    fn typed_and_saved(app: &mut App, text: &str) -> (i64, i64) {
        app.compose_new();
        type_into_body(app, text);
        let t = app.clock + 1.0;
        app.tick(t, &mut Tx::default());
        app.tick(t + SAVE_WHEN_IDLE + 0.1, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert!(!c.dirty, "handed to the engine");
        (c.draft_id, c.submitted)
    }

    #[test]
    fn draft_saved_shows_only_once_the_engine_confirms() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let (id, rev) = typed_and_saved(&mut app, "Dear Ana,");
        assert!(!app.compose.as_ref().unwrap().shows_saved(), "handed over isn't saved");
        app.apply(Event::DraftSaved { id, rev }, &mut Tx::default());
        assert!(app.compose.as_ref().unwrap().shows_saved());
        assert!(app.pending_saves.is_empty(), "nothing left to write on close");
    }

    #[test]
    fn a_failed_save_says_so_and_is_tried_again() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let (id, rev) = typed_and_saved(&mut app, "Dear Ana,");
        app.apply(Event::DraftSaveFailed { id, rev, error: "disk full".into() }, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert!(!c.shows_saved() && c.save_error.as_deref() == Some("Not saved: disk full"));
        let t = app.clock;
        app.tick(t + SAVE_RETRY / 2.0, &mut Tx::default());
        assert_eq!(app.compose.as_ref().unwrap().submitted, rev, "waits a little");
        app.tick(t + SAVE_RETRY, &mut Tx::default());
        let again = app.compose.as_ref().unwrap().submitted;
        assert!(again > rev && app.pending_saves[0].rev == again, "tried again");
        app.apply(Event::DraftSaved { id, rev: again }, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert!(c.shows_saved() && c.save_error.is_none());
    }

    #[test]
    fn a_failed_save_isnt_retried_once_the_message_is_sent() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let (id, rev) = typed_and_saved(&mut app, "Old text");
        app.apply(Event::DraftSaveFailed { id, rev, error: "disk full".into() }, &mut Tx::default());
        // Newer text is sent before the retry comes due.
        app.compose.as_mut().unwrap().body = TextEdit::new("Newer text");
        app.compose.as_mut().unwrap().to = TextEdit::new("ana@x.com");
        app.send_compose();
        let t = app.clock;
        app.tick(t + SAVE_RETRY + 1.0, &mut Tx::default());
        assert!(app.pending_saves.iter().all(|p| p.id != id), "the old text isn't saved over what was sent");
        assert!(matches!(&app.unsaved()[..], [Unsaved::Unsent { draft, .. }] if draft.body == "Newer text"));
    }

    #[test]
    fn a_draft_that_cant_be_written_on_close_goes_to_the_recovery_file() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let (a, _) = typed_and_saved(&mut app, "Draft A");
        app.compose_new();
        type_into_body(&mut app, "Draft B");
        let mut recovered = Vec::new();
        // The database refuses A (disk full, say); B goes in.
        app.close_with(|u| if matches!(u, Unsaved::Save { id, .. } if *id == a) { Err("disk I/O error".to_owned()) } else { Ok(true) }, |failed| {
            recovered = failed;
            Ok(())
        });
        assert!(matches!(&recovered[..], [Unsaved::Save { id, draft, .. }] if *id == a && draft.body == "Draft A"), "{recovered:?}");
    }

    #[test]
    fn a_refused_save_carries_on_as_a_new_draft() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        // Say a too-late Undo put a discarded draft back in the composer after it was deleted.
        let (id, rev) = typed_and_saved(&mut app, "Typed after the undo");
        app.apply(Event::DraftSaveRefused { id, rev }, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert!(c.draft_id != id && c.lifecycle == 0, "a new draft");
        assert!(!c.shows_saved(), "not saved until the engine says so");
        let p = app.pending_saves.iter().find(|p| p.id == c.draft_id).expect("its save handed over");
        assert!(p.draft.body.starts_with("Typed after the undo") && p.lifecycle == 0 && p.rev == c.submitted);
        assert!(app.pending_saves.iter().all(|p| p.id != id), "nothing more for the old one");
    }

    #[test]
    fn a_write_refused_on_close_is_kept_as_a_new_draft() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let (a, _) = typed_and_saved(&mut app, "Draft A");
        let mut written = Vec::new();
        app.close_with(
            |u| match u {
                Unsaved::Save { id, .. } if *id == a => Ok(false),
                _ => {
                    written.push(u.clone());
                    Ok(true)
                }
            },
            |failed| panic!("nothing for the recovery file: {failed:?}"),
        );
        assert!(matches!(&written[..], [Unsaved::Save { id, draft, lifecycle: 0, .. }] if *id != a && draft.body.starts_with("Draft A")), "{written:?}");
    }

    #[test]
    fn an_opened_draft_saves_in_its_lifecycle_and_after_its_last_save() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        // Back from an undone send: a later lifecycle, saved last by a clock far ahead.
        let back = Draft { account: "demo".into(), subject: "Back".into(), body: "Sent text".into(), ..Draft::default() };
        let far = chrono::Utc::now().timestamp_micros() + 3_600_000_000;
        app.apply(Event::DraftOpened { id: 5, draft: back, at: DraftRev { lifecycle: 2, rev: far }, restored: false, notice: None }, &mut Tx::default());
        type_into_body(&mut app, "More: ");
        let t = app.clock + 1.0;
        app.tick(t, &mut Tx::default());
        app.tick(t + SAVE_WHEN_IDLE + 0.1, &mut Tx::default());
        let p = app.pending_saves.iter().find(|p| p.id == 5).expect("saved");
        assert!(p.lifecycle == 2 && p.rev > far, "lifecycle {}, rev {} after {far}", p.lifecycle, p.rev);
    }

    #[test]
    fn closing_writes_everything_not_confirmed() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // sends go to the engine (there is none: nothing confirms)
        // A saved before B took the composer; the engine hasn't confirmed either save.
        let (a, a_rev) = typed_and_saved(&mut app, "Draft A");
        app.compose_new();
        let b = app.compose.as_ref().unwrap().draft_id;
        type_into_body(&mut app, "Draft B, not saved yet");
        // C was sent, not confirmed queued.
        let (c, _) = typed_and_saved(&mut app, "Draft C");
        app.compose.as_mut().unwrap().to = TextEdit::new("ana@x.com");
        app.send_compose();
        let unsaved = app.unsaved();
        let has = |u: &Unsaved, want: i64| match u {
            Unsaved::Save { id, .. } | Unsaved::Unsent { id, .. } => *id == want,
        };
        // Parking saved it again (no longer open), superseding its autosave.
        assert!(matches!(unsaved.iter().find(|u| has(u, a)), Some(Unsaved::Save { open: false, rev, draft, .. }) if *rev > a_rev && draft.body.starts_with("Draft A")), "A: {unsaved:?}");
        assert!(unsaved.iter().any(|u| has(u, b)), "B, parked with its edits: {unsaved:?}");
        assert!(unsaved.iter().any(|u| matches!(u, Unsaved::Unsent { id, draft, .. } if *id == c && draft.to == "ana@x.com")), "C, as sent: {unsaved:?}");
        assert!(matches!(unsaved.last(), Some(Unsaved::Unsent { .. })), "after C's own earlier save, so the sent version wins");
        // In the recovery file too, it keeps its stamp: it overwrites nothing newer there either.
        let Some(Unsaved::Unsent { rev, .. }) = unsaved.last() else { panic!("{unsaved:?}") };
        let kept = unsaved.last().unwrap().recovered();
        assert!(kept.unsent && kept.rev == *rev, "{kept:?}");
        assert!(app.unsaved().is_empty(), "taken");
        // Confirmed, none of it is written again.
        let (d, d_rev) = typed_and_saved(&mut app, "Draft D");
        app.apply(Event::DraftSaved { id: d, rev: d_rev }, &mut Tx::default());
        assert!(app.unsaved().is_empty());
    }

    #[test]
    fn a_send_that_fails_brings_the_message_back() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let (id, _) = typed_and_saved(&mut app, "Hello");
        app.compose.as_mut().unwrap().to = TextEdit::new("ana@x.com");
        app.send_compose();
        assert!(app.compose.is_none());
        app.apply(Event::SendFailed { draft_id: Some(id), error: "Not sent: no.".into() }, &mut Tx::default());
        let c = app.compose.as_ref().expect("back in the composer");
        assert!(c.draft_id == id && c.body.text.starts_with("Hello") && c.to.text == "ana@x.com");
        assert!(c.dirty, "and saved again");
        assert_eq!(c.error.as_deref(), Some("Not sent: no."), "saying why");
    }

    #[test]
    fn a_discarded_composer_comes_back_with_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        // Untouched: just closes.
        app.compose_new();
        app.discard_compose(&mut tx);
        assert!(app.compose.is_none() && app.toasts.is_empty());
        // Written in: a toast with Undo, which brings it back as it was.
        app.compose_new();
        type_into_body(&mut app, "Dear Ana,");
        let id = app.compose.as_ref().unwrap().draft_id;
        app.discard_compose(&mut tx);
        assert!(app.compose.is_none());
        assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some("Draft discarded"));
        assert!(app.undo_last(&mut tx));
        let c = app.compose.as_ref().expect("back");
        assert_eq!((c.draft_id, c.body.text.starts_with("Dear Ana,")), (id, true));
        assert!(c.dirty, "saved again, as open in the composer");
    }

    #[test]
    fn opening_a_draft_puts_the_composer_away_in_drafts() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.compose_new();
        type_into_body(&mut app, "Unfinished");
        let first = app.compose.as_ref().unwrap().draft_id;
        let other = Draft { account: "demo".into(), subject: "Other".into(), body: "Other text".into(), ..Draft::default() };
        app.show_draft(first + 1, other.clone(), DraftRev::default(), false, None);
        assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some("Saved to Drafts"));
        let c = app.compose.as_ref().unwrap();
        assert_eq!((c.draft_id, c.subject.text.as_str(), c.saved, c.minimized), (first + 1, "Other", true, false));
        // The same draft again (say, Undo send while it's open): the composer keeps its edits.
        type_into_body(&mut app, "More ");
        app.show_draft(first + 1, other, DraftRev::default(), false, None);
        assert!(app.compose.as_ref().unwrap().body.text.starts_with("More "));
    }

    fn drafts_view(app: &mut App) -> Summary {
        app.go(View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() });
        app.list.first().expect("a sample draft").clone()
    }

    #[test]
    fn deleting_a_drafts_row_discards_the_draft_with_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let _ = app.settings.set("behavior.confirmDelete", serde_json::json!(true));
        let row = drafts_view(&mut app);
        app.delete(vec![row.id], &mut tx);
        assert!(app.confirm_delete.is_none(), "Undo stands in for the question");
        assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some("Draft discarded"), "not moved to Trash like a message");
        assert!(!app.list.iter().any(|s| s.id == row.id));
        assert!(app.undo_last(&mut tx));
        assert!(app.list.iter().any(|s| s.id == row.id), "back");
        // Archive doesn't apply to drafts.
        let toasts = app.toasts.len();
        app.archive(vec![row.id], &mut tx);
        assert!(app.list.iter().any(|s| s.id == row.id) && app.toasts.len() == toasts);
    }

    #[test]
    fn discarding_the_open_drafts_row_closes_the_composer_until_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let row = drafts_view(&mut app);
        app.open_message(row.id);
        let id = app.compose.as_ref().unwrap().draft_id;
        type_into_body(&mut app, "More: ");
        app.delete(vec![row.id], &mut tx);
        let key = move_key(&app);
        // The engine resolves the row to the composer's draft.
        app.apply(Event::DraftDiscarded { id, undo: Some(key) }, &mut tx);
        assert!(app.compose.is_none(), "the composer closes with its draft");
        assert!(app.undo_last(&mut tx));
        let c = app.compose.as_ref().expect("back");
        assert!(c.draft_id == id && c.body.text.starts_with("More: "), "as it was");
    }

    #[test]
    fn a_server_draft_opens_with_its_files_or_as_an_unsaved_copy() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let files = vec![DraftAttachment { id: 0, filename: "plan.pdf".into(), mime: "application/pdf".into(), size: 10, ..Default::default() }];
        let formatted = Draft { account: "demo".into(), subject: "Plan".into(), attachments: files.clone(), ..Draft::default() };
        app.show_draft(5, formatted, DraftRev::default(), false, Some(DraftNotice::PlainText));
        let c = app.compose.as_ref().unwrap();
        assert_eq!(c.draft().attachments, files, "saved and sent with its files");
        assert!(c.notice.as_deref().is_some_and(|n| n.contains("formatting")) && c.saved);
        // One Envoy can't edit in place: a copy, nothing in the store until it's edited.
        app.compose = None;
        let signed = Draft { account: "demo".into(), subject: "Signed".into(), ..Draft::default() };
        app.show_draft(0, signed, DraftRev::default(), false, Some(DraftNotice::Copy("it's signed or encrypted".into())));
        let c = app.compose.as_ref().unwrap();
        assert!(c.draft_id != 0 && !c.saved);
        assert!(c.notice.as_deref().is_some_and(|n| n.contains("stays in Drafts as it is")));
    }

    #[test]
    fn the_draft_left_open_comes_back_minimized_unless_another_is_open() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let left = Draft { account: "demo".into(), subject: "Left open".into(), ..Draft::default() };
        app.apply(Event::DraftOpened { id: 5, draft: left.clone(), at: DraftRev::default(), restored: true, notice: None }, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert_eq!((c.draft_id, c.minimized), (5, true));
        // Already writing something else: that stays; the restored one waits in Drafts.
        app.compose_new();
        let writing = app.compose.as_ref().unwrap().draft_id;
        app.apply(Event::DraftOpened { id: 6, draft: left, at: DraftRev::default(), restored: true, notice: None }, &mut Tx::default());
        assert_eq!(app.compose.as_ref().unwrap().draft_id, writing);
    }

    #[test]
    fn a_drafts_row_opens_in_the_composer() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.go(View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() });
        let row = app.list.first().expect("a sample draft").clone();
        app.open_message(row.id);
        assert_eq!(app.open, None, "not the reader");
        let c = app.compose.as_ref().expect("the composer");
        assert_eq!(c.subject.text, row.subject);
        assert!(c.to.text.contains("jordan@northwind.dev"));
    }

    #[test]
    fn demo_mode_lists_the_drafts_it_writes() {
        // No store in demo mode: what it saves is kept for the session, and its Drafts lists it.
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.compose_new();
        let c = app.compose.as_mut().unwrap();
        (c.to, c.subject, c.body) = (TextEdit::new("Sam Ortiz <sam@ortiz.family>"), TextEdit::new("Lunch plans"), TextEdit::new("Tacos on Friday?"));
        let id = c.draft_id;
        app.compose_edited();
        let t = app.clock + 1.0;
        app.tick(t, &mut Tx::default());
        app.tick(t + SAVE_WHEN_IDLE + 0.1, &mut Tx::default());
        // Minimized, then Drafts: there, the newest, beside the sample draft.
        app.compose.as_mut().unwrap().minimized = true;
        app.go(View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() });
        let row = app.list.first().unwrap().clone();
        assert_eq!((row.id, row.subject.as_str(), app.list.len()), (envoy_core::drafts::list_id(id), "Lunch plans", 2));
        assert_eq!(row.to.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Sam Ortiz"]);
        // Its row opens it: this composer, expanded.
        app.open_message(row.id);
        let c = app.compose.as_ref().unwrap();
        assert!(!c.minimized && c.draft_id == id);
        // Put away for another, with a file: still listed, and opens again file and all.
        app.compose.as_mut().unwrap().files.push(file_in(9, "menu.pdf", 1200));
        app.compose_edited();
        app.compose_new();
        assert!(app.list.iter().any(|r| r.id == row.id && r.has_attachments), "{:?}", app.list);
        app.compose = None;
        app.open_message(row.id);
        let c = app.compose.as_ref().unwrap();
        assert_eq!((c.draft_id, c.subject.text.as_str(), c.files.len()), (id, "Lunch plans", 1));
        // Discarded: gone from Drafts.
        app.discard_compose(&mut Tx::default());
        assert_eq!(app.list.len(), 1, "only the sample draft: {:?}", app.list);
    }

    fn demo_drafts_view() -> View {
        View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() }
    }

    #[test]
    fn a_demo_draft_put_away_while_its_files_are_read_gets_them() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let dir = std::env::temp_dir().join(format!("envoy-demo-park-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (notes, gone) = (dir.join("notes.txt"), dir.join("missing.pdf"));
        std::fs::write(&notes, b"hello").unwrap();
        app.compose_new();
        let id = app.compose.as_ref().unwrap().draft_id;
        app.attach_paths(vec![notes, gone]);
        // Another composer before demo mode's 0.8 s reads finish: this one goes to Drafts, files
        // and all, as they are.
        app.compose_new();
        assert_ne!(app.compose.as_ref().unwrap().draft_id, id);
        let t = app.clock;
        for i in 1..=50 {
            app.tick(t + 1.0 + i as f64, &mut Tx::default());
            if app.demo_reads.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(app.demo_reads.is_empty());
        app.compose = None;
        app.go(demo_drafts_view());
        app.open_message(envoy_core::drafts::list_id(id));
        let c = app.compose.as_ref().expect("opened from Drafts");
        assert_eq!(c.draft_id, id);
        assert_eq!((c.files[0].status.clone(), c.files[0].size), (FileStatus::Attached, 5), "read while put away");
        assert!(matches!(&c.files[1].status, FileStatus::Failed(why) if why.contains("missing.pdf")), "{:?}", c.files[1].status);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_a_demo_draft_from_drafts_lasts_and_undo_brings_it_back() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        app.compose_new();
        let c = app.compose.as_mut().unwrap();
        c.subject = TextEdit::new("Lunch plans");
        let row = envoy_core::drafts::list_id(c.draft_id);
        app.compose_edited();
        app.compose_new(); // puts it away in Drafts
        app.compose = None;
        app.go(demo_drafts_view());
        assert!(app.list.iter().any(|r| r.id == row));
        app.delete(vec![row], &mut tx);
        assert!(!app.list.iter().any(|r| r.id == row), "off the list at once");
        // Elsewhere and back: still gone.
        app.go_role(Role::Inbox);
        app.go(demo_drafts_view());
        assert!(!app.list.iter().any(|r| r.id == row), "discarded for good: {:?}", app.list);
        // Undo: back, and it stays back.
        assert!(app.undo_last(&mut tx));
        assert!(app.list.iter().any(|r| r.id == row && r.subject == "Lunch plans"), "{:?}", app.list);
        app.go_role(Role::Inbox);
        app.go(demo_drafts_view());
        assert!(app.list.iter().any(|r| r.id == row), "{:?}", app.list);
        app.open_message(row);
        assert_eq!(app.compose.as_ref().map(|c| c.subject.text.as_str()), Some("Lunch plans"));
    }

    // ── attachments ──

    fn files(app: &App) -> Vec<(String, FileStatus)> {
        app.compose.as_ref().unwrap().files.iter().map(|f| (f.filename.clone(), f.status.clone())).collect()
    }

    fn file_ids(app: &App) -> Vec<i64> {
        app.compose.as_ref().unwrap().files.iter().map(|f| f.id).collect()
    }

    fn sent_commands(app: &App, prefix: &str) -> Vec<String> {
        app.sent.borrow().iter().filter(|c| c.starts_with(prefix)).cloned().collect()
    }

    fn file_in(id: i64, name: &str, size: usize) -> DraftAttachment {
        DraftAttachment { id, filename: name.into(), mime: attach::type_for(name).into(), size, status: FileStatus::Attached }
    }

    /// The engine's answer that the composer's file `i` is in, `size` bytes.
    fn answer(app: &mut App, i: usize, size: usize) {
        let c = app.compose.as_ref().unwrap();
        let (draft, f) = (c.draft_id, c.files[i].clone());
        app.apply(Event::FileAttached { draft, file: DraftAttachment { size, status: FileStatus::Attached, ..f } }, &mut Tx::default());
    }

    /// A composer of the engine's (not demo mode's), with `a.pdf` and `b.png` attached.
    fn composer_with_two_files(app: &mut App) -> i64 {
        crate::demo::populate(app);
        app.demo = false;
        app.compose_new();
        app.attach_paths(vec!["/files/a.pdf".into(), "/files/b.png".into()]);
        answer(app, 0, 1000);
        answer(app, 1, 2000);
        app.compose.as_ref().unwrap().draft_id
    }

    #[test]
    fn attached_files_show_at_once_with_ids_never_given_before() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        app.compose_new();
        app.attach_paths(vec!["/files/Q3 report.pdf".into(), "/files/photo.png".into()]);
        assert_eq!(files(&app), vec![("Q3 report.pdf".into(), FileStatus::Attaching), ("photo.png".into(), FileStatus::Attaching)], "shown at once, as on their way");
        let [a, b] = file_ids(&app)[..] else { panic!() };
        assert!(a > 0 && b > a, "stamps: {a}, {b}");
        // The draft is saved first, so the files have a draft to be listed with.
        let sent = app.sent.borrow().clone();
        let save = sent.iter().position(|c| c.starts_with("SaveDraft"));
        let attach = sent.iter().position(|c| c.starts_with("AttachFiles"));
        assert!(save.is_some() && save < attach, "{sent:?}");
        answer(&mut app, 0, 48_000);
        let draft = app.compose.as_ref().unwrap().draft_id;
        app.apply(Event::AttachFailed { draft, file: b, error: "Couldn't read photo.png: permission denied".into() }, &mut Tx::default());
        assert_eq!(app.compose.as_ref().unwrap().files[0], file_in(a, "Q3 report.pdf", 48_000));
        assert!(matches!(&app.compose.as_ref().unwrap().files[1].status, FileStatus::Failed(why) if why.contains("permission denied")));
        // An answer for another draft changes nothing.
        app.apply(Event::FileAttached { draft: draft + 1, file: file_in(b, "photo.png", 1) }, &mut Tx::default());
        assert!(matches!(app.compose.as_ref().unwrap().files[1].status, FileStatus::Failed(_)));
    }

    #[test]
    fn a_draft_reopened_with_a_file_still_attaching_shows_it_and_hears_it_come_in() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        // Put away while a file was on its way, then opened again: the store still lists it.
        let pending = DraftAttachment { id: 9_000_000_000_000_000, filename: "slow.mov".into(), mime: "video/quicktime".into(), size: 0, status: FileStatus::Attaching };
        let draft = Draft { account: "demo".into(), subject: "Plan".into(), attachments: vec![pending.clone()], ..Draft::default() };
        app.show_draft(5, draft, DraftRev::default(), false, None);
        assert_eq!(files(&app), vec![("slow.mov".into(), FileStatus::Attaching)]);
        app.compose.as_mut().unwrap().to = TextEdit::new("ana@x.com");
        app.send_compose();
        assert_eq!(app.compose.as_ref().unwrap().error.as_deref(), Some("Still attaching slow.mov…"));
        app.apply(Event::FileAttached { draft: 5, file: DraftAttachment { size: 8 << 20, status: FileStatus::Attached, ..pending.clone() } }, &mut Tx::default());
        assert_eq!(files(&app), vec![("slow.mov".into(), FileStatus::Attached)]);
        // And a file added now gets an id after every one the draft has, whatever the clock says.
        app.attach_paths(vec!["/files/more.pdf".into()]);
        assert!(file_ids(&app)[1] > pending.id, "{:?}", file_ids(&app));
    }

    /// What closing the window makes again, through [`App::flush_files`].
    fn flushed(app: &App) -> Vec<FileOp> {
        let mut made = Vec::new();
        app.flush_files(|op| {
            made.push(op.clone());
            Ok(())
        });
        made
    }

    #[test]
    fn closing_makes_the_file_changes_the_engine_may_not_have_got_to() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        assert!(flushed(&app).is_empty(), "both in: nothing to make again");
        let a = file_ids(&app)[0];
        app.attach_paths(vec!["/files/late.pdf".into()]);
        let late = app.compose.as_ref().unwrap().files[2].clone();
        app.remove_file(a);
        assert_eq!(flushed(&app), [FileOp::Attach { draft, file: late, source: FileSource::Path("/files/late.pdf".into()) }, FileOp::Detach { draft, file: a }]);
        // In: no longer made again. (Taking off is: it has no answer, and is harmless twice.)
        answer(&mut app, 1, 10);
        assert_eq!(flushed(&app), [FileOp::Detach { draft, file: a }]);
        // One taken off before it's in isn't attached on close.
        app.attach_paths(vec!["/files/oops.pdf".into()]);
        let oops = file_ids(&app)[2];
        app.remove_file(oops);
        assert_eq!(flushed(&app), [FileOp::Detach { draft, file: a }, FileOp::Detach { draft, file: oops }]);
    }

    #[test]
    fn a_draft_written_again_on_close_takes_its_file_changes() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        let a = file_ids(&app)[0];
        app.attach_paths(vec!["/files/late.pdf".into()]);
        let late = app.compose.as_ref().unwrap().files[2].clone();
        app.remove_file(a);
        // Its save refused on close: written again as a new draft, which gets them.
        let mut written = Vec::new();
        app.close_with(
            |u| match u {
                Unsaved::Save { id, .. } if *id == draft => Ok(false),
                Unsaved::Save { id, .. } => {
                    written.push(*id);
                    Ok(true)
                }
                Unsaved::Unsent { .. } => Ok(true),
            },
            |failed| panic!("nothing for the recovery file: {failed:?}"),
        );
        // (The latest, if more than one of its saves was waiting.)
        let new = *written.last().expect("written again");
        assert!(written.iter().all(|w| *w != draft));
        assert_eq!(flushed(&app), [FileOp::Attach { draft: new, file: late, source: FileSource::Path("/files/late.pdf".into()) }, FileOp::Detach { draft: new, file: a }]);
    }

    #[test]
    fn every_file_taken_off_is_taken_off_again_on_close() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        app.attach_paths((0..100).map(|i| format!("/files/{i}.txt").into()).collect());
        let taken: Vec<i64> = file_ids(&app);
        for id in &taken {
            app.remove_file(*id);
        }
        // No answer says a file is off: none is forgotten.
        assert_eq!(flushed(&app), taken.iter().map(|&file| FileOp::Detach { draft, file }).collect::<Vec<_>>());
    }

    #[test]
    fn the_composer_shows_its_files_as_the_store_has_them() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        let [a, b] = file_ids(&app)[..] else { panic!() };
        app.attach_paths(vec!["/files/new.pdf".into()]);
        let new = file_ids(&app)[2];
        app.remove_file(b);
        // The store, before the engine got to either, and with a file another window added.
        let other = new + 1;
        app.apply(Event::DraftFiles { draft, files: vec![file_in(a, "a.pdf", 1000), file_in(b, "b.png", 2000), file_in(other, "other.txt", 5)] }, &mut Tx::default());
        assert_eq!(
            files(&app),
            vec![("a.pdf".into(), FileStatus::Attached), ("new.pdf".into(), FileStatus::Attaching), ("other.txt".into(), FileStatus::Attached)],
            "the store's files, less one being taken off, and one on its way"
        );
        // A file shown on its way that's in by now (it came in under the id the draft had
        // before): shown attached.
        app.apply(Event::DraftFiles { draft, files: vec![file_in(a, "a.pdf", 1000), file_in(new, "new.pdf", 7)] }, &mut Tx::default());
        assert_eq!(files(&app), vec![("a.pdf".into(), FileStatus::Attached), ("new.pdf".into(), FileStatus::Attached)]);
        assert!(!app.file_ops.iter().any(|op| matches!(op, FileOp::Attach { .. })), "listed: nothing left to make on close");
        // Another draft's files change nothing here.
        app.apply(Event::DraftFiles { draft: draft + 1, files: Vec::new() }, &mut Tx::default());
        assert_eq!(file_ids(&app), [a, new]);
    }

    #[test]
    fn a_message_its_server_wouldnt_take_comes_back_saying_why() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let draft = Draft { account: "demo".into(), subject: "Big".into(), ..Draft::default() };
        let why = "Not sent: this message is 40.0 MB once encoded for sending, and its mail server takes at most 35.0 MB. Remove some attachments and send again.";
        app.apply(Event::DraftOpened { id: 5, draft, at: DraftRev::default(), restored: false, notice: Some(DraftNotice::NotSent(why.into())) }, &mut Tx::default());
        let c = app.compose.as_ref().expect("back in the composer");
        assert_eq!((c.draft_id, c.notice.as_deref()), (5, Some(why)));
    }

    #[test]
    fn a_file_taken_off_leaves_the_draft_in_the_store_too() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        let a = file_ids(&app)[0];
        app.remove_file(a);
        assert_eq!(app.compose.as_ref().unwrap().draft().attachments.iter().map(|f| f.filename.as_str()).collect::<Vec<_>>(), ["b.png"]);
        assert!(sent_commands(&app, "DetachFile").last().is_some_and(|c| c.contains(&format!("draft: {draft}, file: {a}"))), "{:?}", sent_commands(&app, "DetachFile"));
        // An answer still on its way for it doesn't bring it back.
        app.apply(Event::FileAttached { draft, file: file_in(a, "a.pdf", 1000) }, &mut Tx::default());
        assert_eq!(files(&app).len(), 1);
    }

    #[test]
    fn sending_waits_for_every_file_and_refuses_too_much() {
        let mut app = App::new(true);
        let draft = composer_with_two_files(&mut app);
        app.compose.as_mut().unwrap().to = TextEdit::new("ana@x.com");
        app.attach_paths(vec!["/files/slow.mov".into()]);
        app.send_compose();
        assert_eq!(app.compose.as_ref().unwrap().error.as_deref(), Some("Still attaching slow.mov…"));
        let slow = file_ids(&app)[2];
        app.apply(Event::AttachFailed { draft, file: slow, error: "slow.mov is 80.0 MB: more than a message can carry (25 MB)".into() }, &mut Tx::default());
        app.send_compose();
        assert_eq!(app.compose.as_ref().unwrap().error.as_deref(), Some("slow.mov isn't attached: remove it, or attach it again"));
        app.remove_file(slow);
        // Two big ones that fit alone but not together.
        app.attach_paths(vec!["/files/big1.mov".into(), "/files/big2.mov".into()]);
        answer(&mut app, 2, 15 << 20);
        answer(&mut app, 3, 15 << 20);
        app.send_compose();
        let error = app.compose.as_ref().unwrap().error.clone().unwrap();
        assert!(error.starts_with("Attachments total 30.0 MB, over") && error.ends_with("Remove some to send."), "{error}");
        // The server said what it takes: judged by that, the files encoded.
        app.apply(Event::SendLimit { account: "demo".into(), size: 40 << 20 }, &mut Tx::default());
        app.send_compose();
        let error = app.compose.as_ref().unwrap().error.clone().unwrap();
        assert!(error.contains("takes in one message"), "30 MB encoded is over 40 MB: {error}");
        assert!(sent_commands(&app, "Send ").is_empty(), "nothing sent");
        app.remove_file(file_ids(&app)[3]);
        app.send_compose();
        assert!(app.compose.is_none(), "sent");
        assert_eq!(sent_commands(&app, "Send ").len(), 1);
    }

    #[test]
    fn forwarding_takes_the_originals_files_and_pictures_along() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let id = app.list.iter().find(|m| m.has_attachments).expect("a demo message with a file").id;
        app.open_message(id);
        let mut original = app.thread.iter().rev().find(|m| m.body.as_ref().is_some_and(|b| !b.attachments.is_empty())).unwrap().clone();
        // A picture shown inside it too.
        let picture = envoy_core::model::Attachment { index: 1, filename: "image-2.png".into(), mime: "image/png".into(), size: 900 };
        original.body.as_mut().unwrap().inline = vec![picture];
        let at = app.thread.iter().position(|m| m.summary.id == original.summary.id).unwrap();
        app.thread[at] = original.clone();
        let mut names: Vec<String> = original.body.as_ref().unwrap().attachments.iter().map(|a| a.filename.clone()).collect();
        names.push("image-2.png".into());
        // Demo mode: they're there at once.
        app.compose_reply(Some(original.summary.id), false, true);
        assert_eq!(app.compose.as_ref().unwrap().draft().attachments.iter().map(|a| a.filename.clone()).collect::<Vec<_>>(), names);
        // With the engine: fetched into the draft, which is saved first.
        app.demo = false;
        app.compose = None;
        app.compose_reply(Some(original.summary.id), false, true);
        assert!(files(&app).iter().all(|(_, s)| *s == FileStatus::Attaching));
        let fwd = sent_commands(&app, "AttachForwarded");
        assert!(fwd.len() == 1 && fwd[0].contains(&format!("message: {}", original.summary.id)) && fwd[0].contains("image-2.png"), "{fwd:?}");
        assert!(app.compose.as_ref().unwrap().saved);
        // Kept in mind until the engine says they're listed, in case the window closes first.
        let from: Vec<FileSource> = app.file_ops.iter().filter_map(|op| if let FileOp::Attach { source, .. } = op { Some(source.clone()) } else { None }).collect();
        assert_eq!(from.len(), names.len());
        assert!(from.iter().all(|s| matches!(s, FileSource::Message { id, .. } if *id == original.summary.id)), "{from:?}");
        // A reply doesn't.
        app.compose = None;
        app.compose_reply(Some(original.summary.id), false, false);
        assert!(app.compose.as_ref().unwrap().files.is_empty());
    }

    #[test]
    fn a_draft_carried_on_under_a_new_id_takes_its_files() {
        let mut app = App::new(true);
        let old = composer_with_two_files(&mut app);
        let t = app.clock + 1.0;
        app.tick(t, &mut Tx::default());
        app.tick(t + SAVE_WHEN_IDLE + 0.1, &mut Tx::default());
        let rev = app.compose.as_ref().unwrap().submitted;
        app.apply(Event::DraftSaveRefused { id: old, rev }, &mut Tx::default());
        let c = app.compose.as_ref().unwrap();
        assert_ne!(c.draft_id, old);
        assert_eq!(c.files.len(), 2, "still shown");
        let saves = sent_commands(&app, "SaveDraftFrom");
        assert!(saves.last().is_some_and(|s| s.contains(&format!("files_from: {old}"))), "{saves:?}");
        // On close, unconfirmed, it's written with where its files are.
        let unsaved = app.unsaved();
        assert!(unsaved.iter().any(|u| matches!(u, Unsaved::Save { files_from: Some(f), .. } if *f == old)), "{unsaved:?}");
    }

    #[test]
    fn demo_mode_reads_file_sizes_off_the_ui_thread() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.compose_new();
        let dir = std::env::temp_dir().join(format!("envoy-demo-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (small, big) = (dir.join("notes.txt"), dir.join("video.mov"));
        std::fs::write(&small, b"hello").unwrap();
        std::fs::File::create(&big).unwrap().set_len(attach::LIMIT as u64 + 1).unwrap();
        app.attach_paths(vec![small, dir.clone(), big]);
        assert!(files(&app).iter().all(|f| f.1 == FileStatus::Attaching));
        // Its threads are quick; the chips wait their moment, then say what they found.
        let t = app.clock;
        for i in 1..=50 {
            app.tick(t + 1.0 + i as f64, &mut Tx::default());
            if app.demo_reads.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let c = app.compose.as_ref().unwrap();
        assert_eq!((c.files[0].status.clone(), c.files[0].size), (FileStatus::Attached, 5));
        assert!(matches!(&c.files[1].status, FileStatus::Failed(why) if why.contains("is a folder")));
        assert!(matches!(&c.files[2].status, FileStatus::Failed(why) if why.contains("more than a message can carry")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn press(app: &mut App, key: rune::Key, mods: Mods) -> bool {
        app.on_key(&KeyEvent { key, mods }, &mut Tx::default())
    }

    fn key(app: &mut App, name: &str) -> bool {
        press(app, rune::Key::Text(name.into()), Mods::default())
    }

    fn shift(app: &mut App, name: &str) -> bool {
        press(app, rune::Key::Text(name.into()), Mods { shift: true, ..Mods::default() })
    }

    /// mod+`c`: ⌘ on macOS, Ctrl elsewhere.
    fn primary(app: &mut App, c: char) -> bool {
        let mac = cfg!(target_os = "macos");
        press(app, rune::Key::Command(c), Mods { cmd: true, ctrl: !mac, logo: mac, ..Mods::default() })
    }

    fn ids(list: &[Summary]) -> Vec<MessageId> {
        list.iter().map(|s| s.id).collect()
    }

    fn sent_since(app: &App, from: usize, kind: &str) -> Vec<String> {
        app.sent.borrow()[from..].iter().filter(|c| c.starts_with(kind)).cloned().collect()
    }

    fn inbox_counts(app: &App) -> (u32, u32) {
        let m = app.mailboxes["demo"].iter().find(|m| m.role == Role::Inbox).unwrap();
        (m.unread, m.total)
    }

    #[test]
    fn the_keyboard_selects_rows_and_one_undo_takes_the_whole_batch_back() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let all = ids(&app.list);
        app.open_message(all[0]);
        // x on the open row, then shift+j twice.
        assert!(key(&mut app, "x"));
        assert!(shift(&mut app, "J") && shift(&mut app, "J"));
        assert_eq!(app.selection.ids(&app.list), all[..3]);
        // j moves the keyboard's row without opening anything; x adds it.
        let open = app.open;
        assert!(key(&mut app, "j"));
        assert_eq!((app.open, app.selection.cursor()), (open, Some(all[3])));
        assert!(key(&mut app, "x"));
        assert_eq!(app.targets(), all[..4]);
        // e archives all four in one command, under one undo key, with one toast.
        let from = app.sent.borrow().len();
        assert!(key(&mut app, "e"));
        let moves = sent_since(&app, from, "Move");
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert!(moves[0].contains(&format!("ids: {:?}, to: Archive", &all[..4])), "{}", moves[0]);
        assert!(app.selection.is_empty(), "the archived rows leave the selection");
        assert_eq!(ids(&app.list), all[4..]);
        assert_eq!(app.toasts.iter().filter(|t| t.undo.is_some()).map(|t| t.text.as_str()).collect::<Vec<_>>(), ["Archived 4"]);
        // z: all four back where they were, and the open message with them.
        assert!(key(&mut app, "z"));
        assert_eq!(ids(&app.list), all);
        assert_eq!(app.open, Some(all[0]));
        assert_eq!(sent_since(&app, from, "UndoMove").len(), 1);
    }

    #[test]
    fn tests_never_see_the_settings_of_the_machine_they_run_on() {
        let mut app = App::new(true);
        let home = crate::test_home();
        use envoy_core::paths;
        for file in [paths::settings_file(), paths::accounts_file(), paths::database_file(), envoy_core::drafts::recovery_dir(), paths::export_dir("html").0] {
            assert!(file.starts_with(home), "{} isn't in the tests' folder {}", file.display(), home.display());
        }
        // An app the engine drives checks settings.json as it ticks: here, the tests' own, which
        // has none, never the one of whoever runs them.
        app.demo = false;
        let _ = app.settings.set("reading.markAsRead", serde_json::json!("delay"));
        app.tick(app.clock + 30.0, &mut Tx::default());
        assert_eq!(app.settings.value("reading.markAsRead"), serde_json::json!("delay"), "not reloaded from anyone's settings.json");
        assert!(app.toasts.is_empty(), "nothing said about settings");
    }

    #[test]
    fn selecting_holds_off_marking_the_hidden_message_read() {
        let unread = |app: &App, id| app.list.iter().find(|s| s.id == id).unwrap().thread_unread;
        // "After a delay": the timer is running when a selection hides the reader.
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // the conversation comes from the engine
        let _ = app.settings.set("reading.markAsRead", serde_json::json!("delay"));
        let mut tx = Tx::default();
        let id = app.list.iter().find(|s| s.thread_unread).unwrap().id;
        app.open_message(id);
        let req = app.thread_req;
        app.apply(Event::Thread { req, id, messages: crate::demo::thread_for(id) }, &mut tx);
        assert!(app.mark_read_at.is_some());
        let other = app.list.iter().find(|s| s.id != id).unwrap().id;
        app.toggle_selected(other);
        app.tick(app.clock + 30.0, &mut tx);
        assert!(unread(&app, id), "not read while the selection hides it");
        // The reader shows it again: the delay starts over, then it's read.
        assert!(press(&mut app, rune::Key::Escape, Mods::default()));
        let t = app.clock;
        app.tick(t + 0.1, &mut tx);
        assert!(unread(&app, id), "a fresh delay, not the old one");
        app.tick(t + 0.2 + app.settings.f64("reading.markAsReadDelay"), &mut tx);
        assert!(!unread(&app, id), "read once shown for the delay");

        // "Immediately": the conversation arrives while rows are selected.
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let id = app.list.iter().find(|s| s.thread_unread).unwrap().id;
        app.open_message(id);
        assert!(key(&mut app, "x"), "selects the open row");
        let req = app.thread_req;
        app.apply(Event::Thread { req, id, messages: crate::demo::thread_for(id) }, &mut tx);
        app.tick(app.clock + 30.0, &mut tx);
        assert!(unread(&app, id), "not read: the reader shows the selection");
        assert!(key(&mut app, "x"), "unselected: the reader shows it again");
        assert!(!unread(&app, id), "and it's read now");
    }

    #[test]
    fn escape_and_select_all() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.open_message(app.list[2].id);
        assert!(key(&mut app, "x"));
        assert!(press(&mut app, rune::Key::Escape, Mods::default()));
        assert!(app.selection.is_empty());
        assert_eq!(app.open, Some(app.list[2].id), "the open message stays open");
        assert!(primary(&mut app, 'a'));
        assert_eq!(app.selection.len(), app.list.len());
        assert!(press(&mut app, rune::Key::Escape, Mods::default()) && app.selection.is_empty());
        // Elsewhere than the mail list, mod+a isn't Envoy's.
        app.screen = Screen::Settings;
        assert!(!primary(&mut app, 'a'));
    }

    #[test]
    fn read_star_and_spam_keys_apply_to_the_whole_selection() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let all = ids(&app.list);
        // Two unread rows and a read, starred one.
        for id in &all[..3] {
            app.toggle_selected(*id);
        }
        assert_eq!(inbox_counts(&app).0, 3);
        let from = app.sent.borrow().len();
        assert!(key(&mut app, "u"), "any unread: all read");
        assert!(app.list[..3].iter().all(|s| !s.thread_unread));
        assert!(sent_since(&app, from, "SetFlags")[0].contains(&format!("ids: {:?}, add: 1, remove: 0", &all[..3])));
        assert_eq!(inbox_counts(&app).0, 1, "the sidebar counts the two read");
        assert!(key(&mut app, "u"), "all read: all unread");
        assert!(app.list[..3].iter().all(|s| s.thread_unread));
        assert!(key(&mut app, "s"), "any unstarred: all starred");
        assert!(app.list[..3].iter().all(|s| s.flagged()));
        assert!(key(&mut app, "s"), "all starred: none");
        assert!(app.list[..3].iter().all(|s| !s.flagged()));
        assert_eq!(app.selection.len(), 3, "marks keep the selection");
        // ! reports them all as spam, at once and undoably.
        let from = app.sent.borrow().len();
        assert!(shift(&mut app, "!"));
        assert!(sent_since(&app, from, "Move")[0].contains("to: Junk"));
        assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some("Marked 3 as spam"));
    }

    #[test]
    fn spam_from_spam_is_not_spam() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut row = app.list[0].clone();
        row.mailbox = "[Gmail]/Spam".into();
        app.list = vec![row.clone()];
        app.open = Some(row.id);
        let from = app.sent.borrow().len();
        assert!(shift(&mut app, "!"));
        assert!(sent_since(&app, from, "Move")[0].contains("to: Inbox"));
    }

    #[test]
    fn the_selection_follows_the_list_by_id_and_ends_with_the_view() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let mut tx = Tx::default();
        app.list = long_list(&app, 10);
        for id in [2, 3, 5] {
            app.toggle_selected(id);
        }
        // Read again: 3 went (moved on another device), new mail came on top.
        app.reload();
        let mut fresh: Vec<Summary> = long_list(&app, 10).into_iter().filter(|s| s.id != 3).collect();
        fresh.insert(0, Summary { id: 99, date: 2_000_000, ..fresh[0].clone() });
        app.apply(Event::List { req: app.list_req, items: fresh, more: false }, &mut tx);
        assert_eq!(app.selection.ids(&app.list), vec![2, 5]);
        // A search's first page isn't the whole list: nothing drops out until the rest comes.
        app.applied_query = "x".into();
        app.request_list();
        app.apply(Event::List { req: app.list_req, items: long_list(&app, 10)[..1].to_vec(), more: true }, &mut tx);
        assert_eq!(app.selection.len(), 2);
        app.apply(Event::List { req: app.list_req, items: long_list(&app, 10)[..3].to_vec(), more: false }, &mut tx);
        assert_eq!(app.selection.ids(&app.list), vec![2], "5 isn't in the results");
        // Another view: nothing selected there.
        app.go(View::Starred { account: None });
        assert!(app.selection.is_empty());
    }

    #[test]
    fn shift_and_mod_clicks_select_and_a_plain_click_opens() {
        let mut app = App::new(true);
        let _ = app.settings.set("inbox.split", serde_json::json!(false)); // the whole inbox
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let all = ids(&app.list);
        app.open_message(all[1]);
        let mod_click = Mods { cmd: true, ..Mods::default() };
        // Mod-click beside the open message picks both.
        app.press_row(all[3], mod_click, &mut tx);
        assert_eq!(app.selection.ids(&app.list), vec![all[1], all[3]]);
        // Shift-click: the range from the last one picked.
        app.press_row(all[5], Mods { shift: true, ..Mods::default() }, &mut tx);
        assert_eq!(app.selection.ids(&app.list), vec![all[1], all[3], all[4], all[5]]);
        app.press_row(all[4], mod_click, &mut tx);
        assert_eq!(app.selection.ids(&app.list), vec![all[1], all[3], all[5]]);
        // A plain click opens the row and ends the selection.
        app.press_row(all[6], Mods::default(), &mut tx);
        assert!(app.selection.is_empty());
        assert_eq!(app.open, Some(all[6]));
    }

    #[test]
    fn the_sidebar_counts_follow_reads_moves_and_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let (unread, total) = inbox_counts(&app);
        // Opening an unread conversation marks it read (demo: at once).
        let first = app.list.iter().find(|s| s.thread_unread).unwrap().id;
        app.open_message(first);
        assert_eq!(inbox_counts(&app), (unread - 1, total), "as the list header says");
        // Archiving an unread conversation takes it, and its messages, off the inbox.
        let row = app.list.iter().find(|s| s.thread_unread).unwrap().clone();
        app.archive(vec![row.id], &mut tx);
        assert_eq!(inbox_counts(&app), (unread - 2, total - row.thread_count));
        assert!(app.undo_last(&mut tx));
        assert_eq!(inbox_counts(&app), (unread - 1, total), "undo puts them back");
        // A move the engine found nothing to do for gives its counts back too.
        app.archive(vec![row.id], &mut tx);
        let key = move_key(&app);
        app.apply(Event::Held { undo: key, ops: 0 }, &mut tx);
        assert_eq!(inbox_counts(&app), (unread - 1, total));
        // Marked unread again: counted again.
        app.toggle_read(first);
        assert_eq!(inbox_counts(&app), (unread, total));
    }

    #[test]
    fn a_list_read_before_a_change_doesnt_undo_it_on_screen() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        let mut tx = Tx::default();
        app.list = long_list(&app, 5);
        // A sync asked for the list; before it comes, row 2 is archived.
        app.reload();
        let stale = app.list_req;
        app.archive(vec![2], &mut tx);
        assert!(app.list_req > stale, "asked again, after the archive");
        // The list read before the archive arrives: it still has row 2.
        app.apply(Event::List { req: stale, items: long_list(&app, 5), more: false }, &mut tx);
        assert!(!app.list.iter().any(|s| s.id == 2), "the archived row doesn't come back");
        app.apply(Event::List { req: app.list_req, items: long_list(&app, 5).into_iter().filter(|s| s.id != 2).collect(), more: false }, &mut tx);
        assert!(!app.list_pending);
        // Nothing on its way: nothing extra to ask for (the engine's reload follows the change).
        let req = app.list_req;
        app.archive(vec![3], &mut tx);
        app.set_read(vec![4], true);
        assert_eq!(app.list_req, req);
        // A read mark made while a list is on its way isn't undone by it either.
        app.reload();
        let stale = app.list_req;
        app.set_read(vec![5], false);
        assert!(app.list_req > stale);
    }

    /// The demo account, and a second, non-Gmail one with a Work folder too; a row in each
    /// account's inbox.
    fn two_accounts(app: &mut App) -> (MessageId, MessageId) {
        let mut other = crate::demo::account();
        other.id = "fm".into();
        other.imap_host = "imap.fastmail.com".into();
        app.accounts.push(other);
        let boxes = [("INBOX", Role::Inbox), ("Archive", Role::Archive), ("Junk", Role::Junk), ("Trash", Role::Trash), ("Work", Role::Other), ("Clients", Role::Other)];
        app.mailboxes.insert(
            "fm".into(),
            boxes.iter().map(|(n, r)| Mailbox { account: "fm".into(), name: (*n).into(), display: (*n).into(), role: *r, unread: 0, total: 0, depth: 0, selectable: true, wake: None }).collect(),
        );
        let mine = app.list[0].id;
        app.list.push(Summary { id: 500, account: "fm".into(), mailbox: "INBOX".into(), ..app.list[0].clone() });
        (mine, 500)
    }

    fn option_ids(options: &[ComboboxOption]) -> Vec<&str> {
        options.iter().map(|o| o.id.as_str()).collect()
    }

    #[test]
    fn move_to_offers_what_every_selected_account_has() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let (mine, theirs) = two_accounts(&mut app);
        // One Gmail message in the inbox: everywhere but the inbox, its labels included, and the
        // split inbox's tabs.
        let tabs = ["cat:important", "cat:other", "cat:newsletters", "cat:notifications"];
        let (options, note) = app.picker_options(Picker::Move, &[mine]);
        assert_eq!(option_ids(&options), [&["role:archive", "role:junk", "role:trash"][..], &tabs[..], &["mb:Receipts", "mb:Travel", "mb:Work"][..]].concat());
        assert_eq!(note, None);
        // Both accounts: only what both have, each role resolved in its own account.
        let (options, note) = app.picker_options(Picker::Move, &[mine, theirs]);
        assert_eq!(option_ids(&options), [&["role:archive", "role:junk", "role:trash"][..], &tabs[..], &["mb:Work"][..]].concat());
        assert_eq!(note.as_deref(), Some("Only folders all 2 accounts have"));
        // Moving there: one undoable move, and the toast names the folder.
        app.toggle_selected(mine);
        app.toggle_selected(theirs);
        let from = app.sent.borrow().len();
        app.pick(Picker::Move, "mb:Work", &mut tx);
        assert!(sent_since(&app, from, "Move")[0].contains("to: Mailbox(\"Work\")"));
        assert_eq!(app.toasts.last().map(|t| (t.text.as_str(), t.undo.is_some())), Some(("Moved 2 to Work", true)));
        assert!(!app.list.iter().any(|s| s.id == mine || s.id == theirs));
    }

    #[test]
    fn a_picker_for_a_huge_selection_is_built_in_linear_time() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        // 40 000 selected rows, 300 folders.
        app.list = long_list(&app, 40_000);
        let folder = |i: usize| Mailbox { account: "demo".into(), name: format!("Folder {i}"), display: format!("Folder {i}"), role: Role::Other, unread: 0, total: 0, depth: 0, selectable: true, wake: None };
        app.mailboxes.get_mut("demo").unwrap().extend((0..300).map(folder));
        let ids = ids(&app.list);
        let started = std::time::Instant::now();
        let (options, _) = app.picker_options(Picker::Move, &ids);
        assert_eq!(options.len(), 3 + 4 + 3 + 300, "archive, spam, trash, the four inbox tabs, the three labels, the 300 folders");
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}: quadratic in the selection?", started.elapsed());
        // Asked again (every frame the picker is open): the same answer, not built again.
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert_eq!(app.picker_options(Picker::Move, &ids).0.len(), options.len());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?} for 100 frames", started.elapsed());
    }

    #[test]
    fn picker_options_follow_the_targets_and_the_folders() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let first = app.list[0].id;
        let names = |app: &App, which| app.picker_options(which, &[first]).0.into_iter().map(|o| o.label).collect::<Vec<_>>();
        assert_eq!(names(&app, Picker::Label), ["Receipts", "Travel", "Work"]);
        // A folder appears (the engine reports the account's mailboxes again).
        let mut boxes = app.mailboxes["demo"].clone();
        boxes.push(Mailbox { name: "Taxes".into(), display: "Taxes".into(), ..boxes.last().unwrap().clone() });
        app.apply(Event::Mailboxes { account: "demo".into(), mailboxes: boxes }, &mut Tx::default());
        assert_eq!(names(&app, Picker::Label), ["Receipts", "Travel", "Work", "Taxes"]);
        // Other targets: what they're offered.
        let mut row = app.list[1].clone();
        row.mailbox = "Work".into();
        app.list[1] = row.clone();
        assert!(!app.picker_options(Picker::Label, &[row.id]).0.iter().any(|o| o.id == "mb:Work"), "it's in Work already");
    }

    #[test]
    fn labels_are_gmail_copies_with_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let (mine, theirs) = two_accounts(&mut app);
        // Not Gmail: no labels, and a word on what to do instead.
        app.open = Some(theirs);
        assert!(key(&mut app, "l"));
        assert!(!app.label_picker.open);
        assert!(app.toasts.last().unwrap().text.starts_with("Labels are for Gmail accounts"));
        // Gmail: the labels (not the folders' roles), the one it's in left out.
        app.open = Some(mine);
        assert!(key(&mut app, "l"));
        assert!(app.label_picker.open && app.label_picker.search.focused);
        assert_eq!(option_ids(&app.picker_options(Picker::Label, &[mine]).0), ["mb:Receipts", "mb:Travel", "mb:Work"]);
        let from = app.sent.borrow().len();
        app.pick(Picker::Label, "mb:Travel", &mut tx);
        let copies = sent_since(&app, from, "Copy");
        assert!(copies[0].contains(&format!("ids: [{mine}], mailbox: \"Travel\", thread: true, undo: Some(")), "{copies:?}");
        assert!(app.list.iter().any(|s| s.id == mine), "it stays where it is");
        assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some("Labeled \u{201c}Travel\u{201d}"));
        // Undo calls the copy back.
        assert!(key(&mut app, "z"));
        assert_eq!(sent_since(&app, from, "UndoMove").len(), 1);
        // v opens Move to… for the open message.
        assert!(key(&mut app, "v") && app.move_picker.open);
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

    // ── snoozes ──

    #[test]
    fn b_snoozes_the_open_message_with_undo() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let all = ids(&app.list);
        app.open_message(all[0]);
        assert!(key(&mut app, "b"));
        assert!(app.snooze_picker.open && app.snooze_picker.search.focused, "its field ready for a typed time");
        let options = app.snooze_options();
        let tomorrow = options.iter().find(|o| o.id.starts_with("tomorrow:")).expect("Tomorrow offered");
        assert_eq!(tomorrow.label, "Tomorrow");
        assert!(options.last().is_some_and(|o| o.disabled && o.label == "Pick date & time…"), "how to type one");
        let at: i64 = tomorrow.id.rsplit(':').next().unwrap().parse().unwrap();
        let from = app.sent.borrow().len();
        app.pick_snooze(&tomorrow.id.clone(), &mut tx);
        let snoozes = sent_since(&app, from, "Snooze");
        assert_eq!(snoozes.len(), 1, "{snoozes:?}");
        assert!(snoozes[0].contains(&format!("ids: [{}], until: {at}, thread: true, undo: Some(", all[0])), "{}", snoozes[0]);
        assert_eq!(ids(&app.list), all[1..], "off the list at once");
        let toast = app.toasts.iter().find(|t| t.undo.is_some()).map(|t| t.text.clone()).unwrap_or_default();
        assert_eq!(toast, format!("Snoozed until {}", fmt::snooze_until(at)));
        // z: back where it was, and the engine takes the snooze back.
        assert!(key(&mut app, "z"));
        assert_eq!(ids(&app.list), all);
        assert_eq!(sent_since(&app, from, "UndoMove").len(), 1);
    }

    #[test]
    fn a_typed_time_is_offered_first_and_nonsense_is_said_to_be() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.snooze_picker.search = TextEdit::new("in 3 days");
        let first = app.snooze_options().remove(0);
        assert!(first.id.starts_with("typed:") && !first.disabled && first.detail == "in 3 days", "{first:?}");
        app.snooze_picker.search = TextEdit::new("whenever");
        let first = app.snooze_options().remove(0);
        assert!(first.disabled && first.detail == "whenever", "{first:?}");
    }

    #[test]
    fn a_selection_snoozes_together_and_drafts_never() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        let all = ids(&app.list);
        app.toggle_selected(all[1]);
        app.toggle_selected(all[2]);
        let from = app.sent.borrow().len();
        app.snooze(app.targets(), 2_000_000_000, &mut tx);
        assert_eq!(sent_since(&app, from, "Snooze").len(), 1);
        assert!(app.toasts.iter().any(|t| t.text.starts_with("Snoozed 2 until ")));
        // Drafts aren't snoozed.
        app.view = View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() };
        app.list = crate::demo::list_for(&app.view);
        app.open = app.list.first().map(|s| s.id);
        let from = app.sent.borrow().len();
        assert!(key(&mut app, "b"));
        assert!(!app.snooze_picker.open || sent_since(&app, from, "Snooze").is_empty());
        app.snooze(app.targets(), 2_000_000_000, &mut tx);
        assert!(sent_since(&app, from, "Snooze").is_empty());
    }

    #[test]
    fn the_snoozed_view_says_when_each_wakes_and_unsnoozes() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        assert!(app.has_snoozed(Some("demo")));
        app.go(View::Role { account: Some("demo".into()), role: Role::Snoozed });
        assert_eq!(app.view_title(), "Snoozed");
        let wakes: Vec<i64> = app.list.iter().map(|s| app.wake_of(&s.account, &s.mailbox).unwrap()).collect();
        assert!(wakes.len() == 2 && wakes[0] <= wakes[1], "soonest first: {wakes:?}");
        let from = app.sent.borrow().len();
        let first = app.list[0].id;
        app.unsnooze(vec![first], &mut tx);
        assert!(sent_since(&app, from, "Move")[0].contains("to: Inbox"));
        assert!(app.toasts.iter().any(|t| t.text == "Unsnoozed"));
    }

    #[test]
    fn in_snoozed_each_row_is_one_message_acted_on_alone() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false; // to see the list asked for
        let mut tx = Tx::default();
        let from = app.sent.borrow().len();
        app.go(View::Role { account: Some("demo".into()), role: Role::Snoozed });
        assert!(sent_since(&app, from, "List").iter().all(|c| c.contains("conversations: false")), "{:?}", sent_since(&app, from, "List"));
        app.demo = true;
        app.go(View::Role { account: Some("demo".into()), role: Role::Inbox });
        app.go(View::Role { account: Some("demo".into()), role: Role::Snoozed });
        let (first, second) = (app.list[0].id, app.list[1].id);
        let from = app.sent.borrow().len();
        app.unsnooze(vec![first], &mut tx);
        app.snooze(vec![second], 2_000_000_000, &mut tx);
        let sent = sent_since(&app, from, "");
        assert!(sent.iter().any(|c| c.starts_with("Move") && c.contains("thread: false")), "{sent:?}");
        assert!(sent.iter().any(|c| c.starts_with("Snooze") && c.contains("thread: false")), "{sent:?}");
    }

    #[test]
    fn what_the_server_snoozed_itself_isnt_moved_from_here() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let mut tx = Tx::default();
        app.mailboxes.get_mut("demo").unwrap().push(Mailbox { account: "demo".into(), name: "Snoozed by server".into(), display: "Snoozed".into(), role: Role::Snoozed, unread: 0, total: 1, depth: 0, selectable: true, wake: None });
        let id = app.list[0].id;
        app.list[0].mailbox = "Snoozed by server".into();
        let from = app.sent.borrow().len();
        app.archive(vec![id], &mut tx);
        app.snooze(vec![id], 2_000_000_000, &mut tx);
        assert!(sent_since(&app, from, "Move").is_empty() && sent_since(&app, from, "Snooze").is_empty());
        assert!(app.list.iter().any(|s| s.id == id));
        assert!(app.toasts.iter().any(|t| t.text.contains("change it there")));
    }

    #[test]
    fn going_to_another_folder_leaves_a_read_woken_message_dealt_with() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let first = app.list[0].id;
        app.list[0].flags |= flags::WOKEN;
        app.open_message(first);
        app.list[0].thread_unread = false; // read
        let from = app.sent.borrow().len();
        app.go_role(Role::Sent);
        let unpinned = sent_since(&app, from, "SetFlags");
        assert!(unpinned.iter().any(|c| c.contains(&format!("ids: [{first}], add: 0, remove: {}", flags::WOKEN))), "{unpinned:?}");
        // Its row gone from the list already (a search, say): the reader's conversation says.
        crate::demo::populate(&mut app);
        let first = app.list[0].id;
        app.open_message(first);
        for m in app.thread.iter_mut() {
            m.summary.flags |= flags::WOKEN | flags::SEEN;
        }
        app.list.clear();
        let from = app.sent.borrow().len();
        app.close_message();
        assert_eq!(sent_since(&app, from, "SetFlags").len(), 1);
    }

    #[test]
    fn a_message_back_from_a_snooze_is_unpinned_once_read_and_left() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let all = ids(&app.list);
        // Back from a snooze, unread: opened (the demo marks it read), then left by j.
        app.list[0].flags |= flags::WOKEN;
        app.open_message(all[0]);
        let from = app.sent.borrow().len();
        app.list[0].thread_unread = false;
        app.open_message(all[1]);
        let unpinned = sent_since(&app, from, "SetFlags");
        assert!(unpinned.iter().any(|c| c.contains(&format!("ids: [{}], add: 0, remove: {}", all[0], flags::WOKEN))), "{unpinned:?}");
        assert_eq!(app.list[0].flags & flags::WOKEN, 0);
        // Left unread: still pinned.
        app.list[2].flags |= flags::WOKEN;
        app.list[2].thread_unread = true;
        app.open = Some(all[2]);
        let from = app.sent.borrow().len();
        app.close_message();
        assert!(sent_since(&app, from, "SetFlags").is_empty());
        assert_ne!(app.list[2].flags & flags::WOKEN, 0);
    }
}
