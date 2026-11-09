//! The engine: a background thread with a tokio runtime that owns the local store, one sync
//! worker per account, and the queue of changes waiting for the server.
//!
//! The UI sends [`Command`]s and drains [`Event`]s; the waker it passes in is called whenever
//! an event is ready, so an idle window stays idle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use base64::Engine as _;
use tokio::sync::{Notify, mpsc};

use crate::accounts::{self, Account, AuthKind};
use crate::imap::{self, Credential, Session};
use crate::model::{Category, Draft, DraftNotice, FileSource, FileStatus, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};
use crate::settings::Settings;
use crate::store::{At, DraftRev, LocalDraft, Op, QueuedOp, Query, ReplyOf, Store};
use crate::{drafts, oauth, paths, secrets, smtp, snooze};

#[derive(Clone, Debug, PartialEq)]
pub enum MoveTarget {
    Archive,
    Trash,
    Junk,
    Inbox,
    Mailbox(String),
}

#[derive(Debug)]
pub enum Command {
    /// `stream`: send the top of a long list ahead of the rest ([`Event::List`]'s `more`), for a
    /// list the user is waiting on (a new view, a search). A list already on screen being read
    /// again (new mail, a sync) goes in one piece, so it never shrinks under the reader.
    /// `category`: a tab of the split inbox, shown when the query has no `category:` of its own.
    /// `counts`: the split inbox's tabs are on screen (with a tab or a search under them): the
    /// view's unread count per category comes with the list ([`Event::CategoryCounts`], under the
    /// same `req`: after its first page, before the whole list).
    List { req: u64, view: View, query: String, conversations: bool, limit: u32, stream: bool, category: Option<Category>, counts: bool },
    /// Moves conversations (those of `ids`) to `category` in the split inbox, by hand, here
    /// only (the server never hears of categories); [`Command::UndoCategory`] with `undo` (a key
    /// as [`Command::Move`]'s) takes it back.
    MoveToCategory { ids: Vec<MessageId>, category: Category, undo: i64 },
    UndoCategory { undo: i64 },
    Open { req: u64, id: MessageId, conversations: bool },
    /// Read a conversation ahead (the list's neighbours of the open one), so the UI can show it
    /// the moment it's opened. Answered with [`Event::Prefetched`]; downloads nothing.
    Prefetch { id: MessageId, conversations: bool },
    /// `thread`: apply to every message of each id's conversation.
    SetFlags { ids: Vec<MessageId>, add: u32, remove: u32, thread: bool },
    /// `thread`: move each id's whole conversation (the part in the same mailbox).
    /// `undo`: a key the caller picks, unique across runs, to undo this action with
    /// [`Command::UndoMove`]; the move then waits `behavior.undoSeconds` before reaching the
    /// server. `None` sends it at once.
    Move { ids: Vec<MessageId>, to: MoveTarget, thread: bool, undo: Option<i64> },
    /// Copies messages to `mailbox`, leaving them where they are: on Gmail, adds that label.
    /// `thread` and `undo` as in [`Command::Move`]: with `undo`, the copies wait out the undo
    /// window (answered with [`Event::Held`]), and [`Command::UndoMove`] takes them back.
    Copy { ids: Vec<MessageId>, mailbox: String, thread: bool, undo: Option<i64> },
    /// Snoozes messages until `until` (Unix seconds): each goes to its account's bucket for that
    /// time ([`crate::snooze`]), as a move held for the undo window like [`Command::Move`]'s
    /// (answered with [`Event::Held`]; [`Command::UndoMove`] takes it back, and then the server
    /// never hears of it). Messages already snoozed are rescheduled. `thread`: each id's whole
    /// conversation, the part in the same mailbox.
    Snooze { ids: Vec<MessageId>, until: i64, thread: bool, undo: Option<i64> },
    /// Takes back a [`Command::Move`] (or [`Command::Copy`], [`Command::Snooze`], or
    /// [`Command::DiscardDraft`]) that hasn't reached the server yet.
    UndoMove { undo: i64 },
    /// Sends after `compose.undoSendSeconds`. `draft_id`: the composer's draft, which goes once
    /// the message is out, and comes back in the composer if the send is undone.
    Send { draft: Draft, draft_id: Option<i64> },
    UndoSend { op: i64 },
    /// Saves the composer's content as draft `id` (an id the app picks, unique across runs) and
    /// queues its upload to the Drafts mailbox. `lifecycle` (the draft's, as the app last heard)
    /// and `rev` order saves ([`Store::save_draft`]); `open`: it's still in the composer.
    /// Answered with [`Event::DraftSaved`], [`Event::DraftSaveFailed`] or
    /// [`Event::DraftSaveRefused`].
    SaveDraft { id: i64, draft: Draft, open: bool, rev: i64, lifecycle: i64 },
    /// [`Command::SaveDraft`] for a draft that carries on draft `files_from`'s text (a save
    /// refused under that id): the files it lists are that draft's ([`Store::save_draft_from`]).
    SaveDraftFrom { id: i64, draft: Draft, open: bool, rev: i64, lifecycle: i64, files_from: i64 },
    /// Attaches files from disk to draft `draft`, with the ids given (the app's stamps: never
    /// given twice). The draft must be in the store (the composer saves it first). Each is
    /// listed with the draft at once, as being attached ([`Store::attaching`]), then read off
    /// the UI thread and the engine's loop; whether the composer is still there or not, the
    /// store gets the file, and a restart takes up one cut off. Each is answered with
    /// [`Event::FileAttached`] or [`Event::AttachFailed`].
    AttachFiles { draft: i64, files: Vec<(i64, PathBuf)> },
    /// Attaches files of message `message` to draft `draft`, as a forward takes them along:
    /// each as listed (id, name, type, size), with its index among the message's attachments
    /// ([`crate::model::Attachment::index`]). A message not downloaded yet is fetched first.
    /// Answered as [`Command::AttachFiles`].
    AttachForwarded { draft: i64, message: MessageId, files: Vec<(crate::model::DraftAttachment, usize)> },
    /// Takes file `file` off draft `draft` ([`Store::remove_file`]).
    DetachFile { draft: i64, file: i64 },
    /// Opens a row of a Drafts list in the composer: answered with [`Event::DraftOpened`].
    OpenDraft { id: MessageId },
    /// Discards draft `id`, here and on the server. `undo` as in [`Command::Move`].
    DiscardDraft { id: i64, undo: Option<i64> },
    /// Deletes a row of a Drafts list: its local draft is discarded (all its copies go), and a
    /// draft only the server has goes to Trash like a message. `undo` as in [`Command::Move`].
    DiscardDraftRow { row: MessageId, undo: Option<i64> },
    SyncNow,
    /// The mailbox the user is looking at: synced first and kept fresh.
    Watch { view: View },
    AddAccount { account: Account, secret: String },
    RemoveAccount { id: String },
    GoogleSignIn { client_id: String, client_secret: String, hint: String },
    SaveAttachment { id: MessageId, index: usize },
    /// Open the message as received: its raw MIME as an `.eml` file in a private folder
    /// ([`crate::paths::write_export`]), for the system's mail viewer.
    OpenOriginal { id: MessageId },
    /// Open a message's HTML in the system browser, sanitized ([`crate::mime::browser_html`]):
    /// nothing in it runs, and remote pictures load only with `remote_images`. Written to the
    /// same private folder.
    OpenInBrowser { id: MessageId, remote_images: bool },
    Settings(Box<Settings>),
    /// Download (or read from the message, for `cid:`) and decode an image in an HTML mail.
    FetchImage { url: String, message: MessageId },
    /// People to suggest for what's typed in a recipients field ([`Store::suggest`]), from the
    /// local cache only. Answered with [`Event::Contacts`] carrying the same `req`.
    Contacts { req: u64, typed: String },
}

#[derive(Debug, Clone)]
pub enum Event {
    Accounts(Vec<Account>),
    Mailboxes { account: String, mailboxes: Vec<Mailbox> },
    /// A list's rows. `more`: these are the top of a longer list, whose whole (starting with
    /// the same rows) follows in another `List` with the same `req`.
    List { req: u64, items: Vec<Summary>, more: bool },
    /// A view's unread conversations (messages, outside conversation view) per category, by
    /// [`Category::code`]: the split inbox's tabs. Sent with list `req`, which asked for them.
    CategoryCounts { req: u64, view: View, counts: [u32; 4] },
    /// An opened message's conversation's category in the split inbox, and why, in words
    /// ([`Store::explain`]). Follows its [`Event::Thread`].
    Explained { id: MessageId, category: Category, why: String },
    Thread { req: u64, id: MessageId, messages: Vec<ThreadMessage> },
    /// A [`Command::Prefetch`]'s conversation, as `Thread` would bring it.
    Prefetched { id: MessageId, messages: Vec<ThreadMessage> },
    /// Something in this account's local copy changed: reload what's on screen.
    Changed { account: String },
    Sync { account: String, state: SyncState },
    Toast(String),
    Error(String),
    /// A send was queued (op `op`), with the composer's draft `draft_id`.
    Sent { op: i64, undo_secs: u32, draft_id: Option<i64> },
    /// A send couldn't even be queued (an [`Event::Error`] says why): the composer should get
    /// draft `draft_id` back.
    SendFailed { draft_id: Option<i64>, error: String },
    /// Save `rev` of draft `id` is written to the local database ([`Command::SaveDraft`]).
    DraftSaved { id: i64, rev: i64 },
    /// Save `rev` of draft `id` couldn't be written: it isn't saved, and should be tried again.
    DraftSaveFailed { id: i64, rev: i64, error: String },
    /// Save `rev` of draft `id` was refused: the draft moved on without it (gone for good, or
    /// sent, since it was made). It isn't saved, and a retry won't be: keep it as a new draft.
    DraftSaveRefused { id: i64, rev: i64 },
    /// Draft `draft`'s files, all of them, as the store has them: after it took on another
    /// draft's files, or when a send found the composer showing other files. File events are
    /// sent in the order the store's changes happened, this too, so the composer can take it as
    /// it comes.
    DraftFiles { draft: i64, files: Vec<crate::model::DraftAttachment> },
    /// A file of draft `draft` is in: attached, saved and sent with it.
    FileAttached { draft: i64, file: crate::model::DraftAttachment },
    /// File `file` couldn't be attached to draft `draft`: `error` says why, to show.
    AttachFailed { draft: i64, file: i64, error: String },
    /// The largest message the account's SMTP server takes, as it said (its EHLO `SIZE`), for
    /// the composer to judge a message by ([`crate::attach::Limit`]).
    SendLimit { account: String, size: u64 },
    /// How many ops an undoable [`Command::Move`] (or [`Command::Copy`]) actually held. 0 means
    /// it had nothing to do (the messages were already moving, or already there), so there's
    /// nothing to undo.
    Held { undo: i64, ops: usize },
    AccountAdded(Account),
    AccountFailed(String),
    GoogleSignedIn { email: String, name: String, refresh_token: String },
    GoogleFailed(String),
    /// A file was written (attachment, original message) and can be opened.
    Saved(PathBuf),
    Pending(u32),
    /// A decoded image: straight-alpha sRGB RGBA.
    Image { url: String, width: u32, height: u32, rgba: Arc<[u8]> },
    ImageFailed { url: String },
    /// A draft for the composer: opened from Drafts, or back from an undone send, or, with
    /// `restored`, the one left in the composer when the app last closed. `notice`: what to
    /// tell the user about a draft from the server; with [`DraftNotice::Copy`] it's an unsaved
    /// copy and `id` is 0, for the app to pick one. `at`: the draft's lifecycle, for its saves
    /// to carry, and its newest save's stamp.
    DraftOpened { id: i64, draft: Draft, at: DraftRev, restored: bool, notice: Option<DraftNotice> },
    /// Draft `id` was discarded from the Drafts list ([`Command::DiscardDraftRow`]), under
    /// `undo`: a composer holding it closes.
    DraftDiscarded { id: i64, undo: Option<i64> },
    /// The people to suggest for [`Command::Contacts`] `req`, best first.
    Contacts { req: u64, contacts: Vec<crate::model::Address> },
}

type Waker = Arc<dyn Fn() + Send + Sync>;

/// How many rows of a long list are sent ahead of the rest: more than a screenful, and what
/// the list mounts around it, but a small part of the work.
pub const FIRST_PAGE: u32 = 100;

/// How many people a [`Command::Contacts`] brings: what the composer shows, and a few more for
/// it to leave out the ones already in the message.
pub const SUGGESTIONS: usize = 12;

/// How many messages of an old cache [`Store::backfill_contacts`] counts at a time: the writer
/// is held about 20 ms a chunk on envoy-bench's 100k-message cache (500 held it 33 ms), and the
/// whole cache takes about 10 s, in the background.
pub const CONTACTS_CHUNK: i64 = 200;

pub struct Engine {
    tx: mpsc::UnboundedSender<Command>,
    rx: std::sync::mpsc::Receiver<Event>,
    writer: OpenedStore,
}

/// The store's writer, once the engine thread has opened it ([`Engine::start`]): for writes
/// that can't wait for the engine ([`Engine::save_draft_now`]).
#[derive(Clone, Default)]
struct OpenedStore(Arc<std::sync::OnceLock<Arc<Mutex<Store>>>>);

impl OpenedStore {
    /// The store, locked: an error if it isn't open (not yet, or it couldn't be).
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        self.0.get().ok_or_else(|| anyhow!("the mail cache isn't open"))?.lock().map_err(|_| anyhow!("store poisoned"))
    }
}

impl Engine {
    /// Starts the engine thread and returns at once: the cache is opened (and an old one
    /// migrated) on that thread, never the caller's, so a UI can start its window meanwhile.
    /// Commands sent before it's open wait their turn. If the cache can't be opened, the engine
    /// reports an [`Event::Error`] and stops; later commands are dropped.
    pub fn start(waker: impl Fn() + Send + Sync + 'static, settings: Settings) -> Result<Engine> {
        let (tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, rx) = std::sync::mpsc::channel();
        let waker: Waker = Arc::new(waker);
        let writer = OpenedStore::default();
        let opened = writer.clone();
        std::thread::Builder::new().name("envoy-engine".into()).spawn(move || {
            let db = paths::database_file();
            let stores = Store::open(&db).and_then(|writer| Ok((writer, Store::open(&db)?)));
            let (writer, reader) = match stores {
                Ok(s) => s,
                Err(e) => {
                    let _ = ev_tx.send(Event::Error(format!("Couldn't open the mail cache: {e:#}")));
                    waker();
                    return;
                }
            };
            let writer = Arc::new(Mutex::new(writer));
            let _ = opened.0.set(writer.clone());
            let shared = Arc::new(Shared {
                writer,
                reader: Arc::new(Mutex::new(reader)),
                events: Mutex::new(ev_tx),
                waker,
                settings: RwLock::new(settings),
                tokens: Mutex::new(HashMap::new()),
                accounts: RwLock::new(accounts::load()),
                watched: Mutex::new(HashSet::new()),
                synced: Mutex::new(HashMap::new()),
                undo_keys: Mutex::new(HashMap::new()),
                wakers: Mutex::new(HashMap::new()),
                category_undo: Mutex::new(HashMap::new()),
                recategorizing: Default::default(),
            });
            shared.store_me();
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("envoy-io").build() {
                Ok(rt) => rt,
                Err(e) => {
                    shared.emit(Event::Error(format!("couldn't start the engine: {e}")));
                    return;
                }
            };
            rt.block_on(run(shared, cmd_rx));
        })?;
        Ok(Engine { tx, rx, writer })
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }

    /// [`Command::SaveDraft`], done on the calling thread before returning: for when the window
    /// closes, since the process may exit before the engine gets to a queued command. Waits only
    /// for the store; the upload goes out on the next start if not before.
    /// False when the store refused it ([`Store::save_draft`]): it isn't saved.
    pub fn save_draft_now(&self, id: i64, draft: &Draft, open: bool, at: DraftRev, files_from: Option<i64>) -> Result<bool> {
        let st = self.writer.lock()?;
        Ok(drafts::save_from(&st, id, draft, open, at, now(), files_from)?.is_some())
    }

    /// Keeps a message the composer sent (at `at`) as a draft, on the calling thread, unless
    /// its send was queued (or went out): for when the window closes before the engine
    /// confirmed it. False when the store refused it ([`Store::draft_unsent`]).
    pub fn keep_unsent_now(&self, id: i64, draft: &Draft, at: DraftRev) -> Result<bool> {
        let st = self.writer.lock()?;
        st.draft_unsent(id, draft, Some(at), now())
    }

    /// [`Command::AttachFiles`] and [`Command::AttachForwarded`]'s listing, on the calling
    /// thread: for when the window closes before the engine got to them. The files are listed
    /// with the draft, being attached, and the next start reads them in. An id the draft
    /// already has changes nothing, so a listing the engine did make is left as it is.
    pub fn attach_now(&self, draft: i64, files: &[(crate::model::DraftAttachment, FileSource)]) -> Result<()> {
        let st = self.writer.lock()?;
        st.attaching(draft, files)?;
        Ok(())
    }

    /// [`Command::DetachFile`], on the calling thread (see [`Engine::attach_now`]).
    pub fn detach_now(&self, draft: i64, file: i64) -> Result<()> {
        let st = self.writer.lock()?;
        st.remove_file(draft, file, now())?;
        Ok(())
    }

    pub fn drain(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }
}

struct Shared {
    writer: Arc<Mutex<Store>>,
    reader: Arc<Mutex<Store>>,
    events: Mutex<std::sync::mpsc::Sender<Event>>,
    waker: Waker,
    settings: RwLock<Settings>,
    tokens: Mutex<HashMap<String, (String, Instant)>>,
    accounts: RwLock<Vec<Account>>,
    /// Mailboxes being looked at, `(account, name)`: synced with the inbox, and in between
    /// looked at for changes ([`changed_since_synced`]).
    watched: Mutex<HashSet<(String, String)>>,
    /// Each mailbox's contents, `(account, name)`, as its last sync on either connection found
    /// them: what [`changed_since_synced`] compares with.
    synced: Mutex<HashMap<(String, String), imap::Contents>>,
    /// Undo keys of moves this run held ops for, with how many: to tell "too late" from
    /// "nothing to undo", and a partial undo from a full one.
    undo_keys: Mutex<HashMap<i64, usize>>,
    /// Each account's worker's [`Worker::notify`], to wake it from off the engine's loop.
    wakers: Mutex<HashMap<String, Arc<Notify>>>,
    /// Corrections in the split inbox this run, by undo key: what takes each back.
    category_undo: Mutex<HashMap<i64, Vec<crate::store::Moved>>>,
    /// A pass sorting the whole cache again is running ([`recategorize`]).
    recategorizing: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn emit(&self, e: Event) {
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(e);
        }
        (self.waker)();
    }

    fn setting_u32(&self, key: &str) -> u32 {
        self.settings.read().map(|s| s.u32(key)).unwrap_or(0)
    }

    fn setting_bool(&self, key: &str) -> bool {
        self.settings.read().map(|s| s.bool(key)).unwrap_or(false)
    }

    fn account(&self, id: &str) -> Option<Account> {
        self.accounts.read().ok()?.iter().find(|a| a.id == id).cloned()
    }

    fn me(&self) -> HashSet<String> {
        self.accounts.read().map(|a| a.iter().map(|a| a.email.to_lowercase()).collect()).unwrap_or_default()
    }

    /// Tells the store your addresses, as accounts come and go ([`Store::set_me`]).
    fn store_me(&self) {
        let me = self.me();
        if let Ok(st) = self.writer.lock() {
            st.set_me(me);
        }
    }

    /// Who an account's mail is from, for rows that have no message to take it from.
    fn sender(&self, account: &str) -> crate::model::Address {
        let a = self.account(account);
        crate::model::Address { name: a.as_ref().map(|a| a.display_name.clone()).unwrap_or_default(), email: a.map(|a| a.email).unwrap_or_default() }
    }

    fn with_writer<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let st = self.writer.lock().map_err(|_| anyhow!("store poisoned"))?;
        f(&st)
    }

    fn emit_mailboxes(&self, account: &str) {
        if let Ok(mailboxes) = self.with_writer(|st| st.mailboxes(account)) {
            self.emit(Event::Mailboxes { account: account.to_owned(), mailboxes });
        }
    }

    /// Records what a sync of `mailbox` found ([`imap::SyncOutcome::contents`]).
    fn synced(&self, account: &str, mailbox: &str, contents: imap::Contents) {
        if let Ok(mut s) = self.synced.lock() {
            s.insert((account.to_owned(), mailbox.to_owned()), contents);
        }
    }

    /// Wakes an account's worker to plan its wait again (from anywhere: see [`wake`]).
    fn wake_account(&self, account: &str) {
        if let Some(n) = self.wakers.lock().ok().and_then(|w| w.get(account).cloned()) {
            n.notify_one();
        }
    }

    /// How big a message from this account may be.
    fn limit(&self, account: &str) -> crate::attach::Limit {
        let server = self.with_writer(|st| st.send_limit(account)).ok().flatten();
        crate::attach::Limit::new(self.account(account).as_ref(), server)
    }

    /// The account's SMTP server said what it takes: kept, and the app told.
    fn learned_size(&self, account: &str, size: Option<u64>) {
        let Some(size) = size else { return };
        if self.with_writer(|st| st.set_send_limit(account, size)).unwrap_or(false) {
            self.emit(Event::SendLimit { account: account.to_owned(), size });
        }
    }

    fn emit_pending(&self) {
        if let Ok(n) = self.with_writer(|st| st.pending_count()) {
            self.emit(Event::Pending(n));
        }
    }

    /// A credential to log in with, refreshing an OAuth access token when it is near expiry.
    async fn credential(&self, account: &Account) -> Result<Credential> {
        let secret = secrets::get(&account.id).ok_or_else(|| anyhow!("No password saved for {}. Remove and add the account again.", account.email))?;
        match account.auth {
            AuthKind::Password => Ok(Credential::Password(secret)),
            AuthKind::GoogleOAuth => {
                if let Some((tok, exp)) = self.tokens.lock().ok().and_then(|t| t.get(&account.id).cloned())
                    && exp > Instant::now()
                {
                    return Ok(Credential::OAuth(tok));
                }
                let (cid, csec) = client_for(account);
                let tokens = tokio::task::spawn_blocking(move || oauth::refresh(&cid, &csec, &secret)).await??;
                if let Ok(mut t) = self.tokens.lock() {
                    t.insert(account.id.clone(), (tokens.access_token.clone(), tokens.expires_at));
                }
                Ok(Credential::OAuth(tokens.access_token))
            }
        }
    }
}

fn client_for(account: &Account) -> (String, String) {
    if account.oauth_client_id.is_empty() { oauth::default_client() } else { (account.oauth_client_id.clone(), account.oauth_client_secret.clone()) }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// An account's two connections: `jobs` feeds the background sync loop; `now` feeds the
/// foreground connection, which serves whatever the user is looking at right away (opening
/// a message or a folder) instead of queuing behind a long background download.
struct Worker {
    jobs: mpsc::UnboundedSender<Job>,
    now: mpsc::UnboundedSender<Job>,
    notify: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
    fg_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
enum Job {
    /// Download these messages now (someone opened them), then answer. `validity`: the
    /// generation of the mailbox (UIDVALIDITY) the UIDs are from, as cached when asked for;
    /// in another, nothing is stored and the mailbox is synced again ([`download_bodies`]).
    Bodies { mailbox: String, validity: u32, uids: Vec<u32>, reply: Option<Reply> },
    SyncMailbox(String),
    Sync,
    /// [`Job::Sync`], with the mailbox list read again (new snooze buckets, folders made elsewhere).
    Folders,
}

impl Job {
    /// What it does, for the debug log (it names the mailbox).
    fn what(&self) -> String {
        match self {
            Job::Bodies { mailbox, uids, .. } => format!("download {} message(s) in {mailbox}", uids.len()),
            Job::SyncMailbox(mailbox) => format!("sync {mailbox}"),
            Job::Sync => "check for mail".to_owned(),
            Job::Folders => "check for mail and folders".to_owned(),
        }
    }

    /// What kind of job it is, naming nothing of the user's: for logs at default levels.
    fn kind(&self) -> &'static str {
        match self {
            Job::Bodies { .. } => "download",
            Job::SyncMailbox(_) => "sync",
            Job::Sync => "check",
            Job::Folders => "check folders",
        }
    }
}

/// An account as logs at default levels name it: a short hash of its id (which is made from its
/// address), the same for the account in every log line, and telling nothing of the address.
fn account_tag(id: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(id.as_bytes());
    format!("account {:02x}{:02x}{:02x}{:02x}", hash[0], hash[1], hash[2], hash[3])
}

/// What to send the UI once [`Job::Bodies`] has the messages.
#[derive(Debug)]
enum Reply {
    /// The open conversation again: `(req, id, conversations)` as in [`Command::Open`].
    Thread(u64, MessageId, bool),
    /// A Drafts row, now readable, in the composer ([`Command::OpenDraft`]).
    Draft(MessageId),
    /// A forwarded message's files, now readable: `(file id, index)` ([`Command::AttachForwarded`]).
    Attach { message: MessageId, files: Vec<(i64, usize)> },
}

/// Answers a [`Job::Bodies`] once its messages are downloaded.
fn reply(shared: &Arc<Shared>, reply: &Reply) -> Result<()> {
    match *reply {
        Reply::Thread(req, id, conversations) => {
            let messages = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?.thread(id, conversations)?;
            shared.emit(Event::Thread { req, id, messages });
        }
        Reply::Draft(id) => {
            open_draft(shared, id, now())?;
        }
        Reply::Attach { message, ref files } => {
            let (shared, files) = (shared.clone(), files.clone());
            tokio::task::spawn_blocking(move || attach_forwarded(&shared, message, &files));
        }
    }
    Ok(())
}

async fn run(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Command>) {
    // Before any worker can replay them.
    drop_unprovable_ops(&shared);
    // Before any worker stores mail: it's sorted with the user's rules as it lands.
    update_classifier(&shared);
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let accounts = shared.accounts.read().map(|a| a.clone()).unwrap_or_default();
    shared.emit(Event::Accounts(accounts.clone()));
    for a in &accounts {
        shared.emit_mailboxes(&a.id);
        workers.insert(a.id.clone(), spawn_worker(shared.clone(), a.id.clone()));
    }
    // Drafts the app couldn't write to the store as it last closed, kept in a file meanwhile.
    match shared.with_writer(|st| drafts::import_recovery(&drafts::recovery_dir(), st, now())) {
        Ok(0) => {}
        Ok(n) => shared.emit(Event::Toast(format!("Recovered {n} draft{} that couldn't be saved when Envoy last closed", if n == 1 { "" } else { "s" }))),
        Err(e) => log::error!("couldn't import the drafts in {}: {e:#}", drafts::recovery_dir().display()),
    }
    // What was being written when the app last closed (or crashed) goes back in the composer.
    // Sent as read, the store held meanwhile: no file event can come between (see [`open_draft`]).
    let restored = shared.with_writer(|st| {
        if let Some(d) = st.draft_to_restore()? {
            shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: true, notice: None });
        }
        Ok(())
    });
    if let Err(e) = restored {
        log::warn!("couldn't read drafts: {e:#}");
    }
    // What the accounts' servers said they take, for the composer.
    for (account, size) in shared.with_writer(|st| st.send_limits()).unwrap_or_default() {
        shared.emit(Event::SendLimit { account, size });
    }
    // Files that were being attached when the app last closed.
    resume_attaching(&shared, &workers);
    shared.emit_pending();
    tokio::spawn(upgrade_search(shared.clone()));
    tokio::spawn(backfill_contacts(shared.clone()));
    while let Some(cmd) = rx.recv().await {
        if let Err(e) = handle(&shared, &mut workers, cmd).await {
            shared.emit(Event::Error(format!("{e:#}")));
        }
    }
}

/// Drops queued changes left by an older Envoy that can't be applied safely
/// ([`Store::drop_unprovable_ops`]), and says so once, however many.
fn drop_unprovable_ops(shared: &Shared) {
    match shared.with_writer(|st| st.drop_unprovable_ops()) {
        Ok(0) => {}
        Ok(n) => {
            log::warn!("dropped {n} queued change(s) from before generations were recorded");
            shared.emit(Event::Error(format!(
                "{} from before the update couldn't be applied safely and {} dropped: the messages they were for may have changed on the server since.",
                if n == 1 { "1 queued change".to_owned() } else { format!("{n} queued changes") },
                if n == 1 { "was" } else { "were" }
            )));
            shared.emit(Event::Changed { account: String::new() });
        }
        Err(e) => log::error!("couldn't check the queued changes: {e:#}"),
    }
}

/// Gives an old cache's search index its prefix index ([`Store::upgrade_search`]), a few
/// hundred messages at a time with the writer free in between, once startup is out of the way.
/// Searches keep using the old index until it's done.
async fn upgrade_search(shared: Arc<Shared>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    loop {
        let s = shared.clone();
        match tokio::task::spawn_blocking(move || s.with_writer(|st| st.upgrade_search(400))).await {
            Ok(Ok(true)) => tokio::time::sleep(Duration::from_millis(25)).await,
            Ok(Ok(false)) => return,
            Ok(Err(e)) => {
                log::warn!("upgrading the search index: {e:#}");
                return;
            }
            Err(_) => return,
        }
    }
}

/// Counts the people in an old cache's mail for recipient suggestions
/// ([`Store::backfill_contacts`]), as [`upgrade_search`] goes: a few hundred messages at a time,
/// the writer free in between, once startup is out of the way.
async fn backfill_contacts(shared: Arc<Shared>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    loop {
        let s = shared.clone();
        match tokio::task::spawn_blocking(move || s.with_writer(|st| st.backfill_contacts(CONTACTS_CHUNK))).await {
            Ok(Ok(true)) => tokio::time::sleep(Duration::from_millis(25)).await,
            Ok(Ok(false)) => return,
            Ok(Err(e)) => {
                log::warn!("counting the cache's contacts: {e:#}");
                return;
            }
            Err(_) => return,
        }
    }
}

/// Gives the store the split inbox's rules (`inbox.rules`) and the accounts' addresses: mail
/// that moves because of it shows at once, and a pass sorting the whole cache again, if one is
/// needed (or was cut off by a quit), runs in the background ([`recategorize`]). Called from a
/// few places, built once.
#[inline(never)]
fn update_classifier(shared: &Arc<Shared>) {
    let Ok(settings) = shared.settings.read().map(|s| s.clone()) else { return };
    let classifier = crate::category::Classifier::from_settings(&settings, shared.me());
    match shared.with_writer(|st| {
        st.set_classifier(classifier)?;
        Ok((st.take_recategorized(), st.recategorizing()?))
    }) {
        Ok((moved, pass)) => {
            if moved > 0 {
                shared.emit(Event::Changed { account: String::new() });
            }
            if pass {
                start_recategorizing(shared);
            }
        }
        Err(e) => log::warn!("couldn't give the store the inbox rules: {e:#}"),
    }
}

fn start_recategorizing(shared: &Arc<Shared>) {
    if !shared.recategorizing.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || recategorize(&shared));
    }
}

/// Sorts the whole cache again ([`Store::recategorize`]), off the engine's loop, a few hundred
/// messages at a time with the writer free in between (a chunk is a few milliseconds), newest
/// first so what's on screen is right first. The list follows after the first chunk that moved
/// anything (a cache from before the split starts all in Other), then a few times a second.
fn recategorize(shared: &Arc<Shared>) {
    let mut shown: Option<Instant> = None;
    let mut unseen = 0;
    loop {
        let pass = match shared.with_writer(|st| st.recategorize(500)) {
            Ok(pass) => pass,
            Err(e) => {
                log::warn!("sorting the inbox again: {e:#}");
                shared.recategorizing.store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        unseen += pass.changed;
        if unseen > 0 && (!pass.more || shown.is_none_or(|t| t.elapsed() >= Duration::from_millis(400))) {
            shared.emit(Event::Changed { account: String::new() });
            (shown, unseen) = (Some(Instant::now()), 0);
        }
        if pass.more {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        shared.recategorizing.store(false, std::sync::atomic::Ordering::SeqCst);
        // Rules that changed again as it ended start another.
        if !shared.with_writer(|st| st.recategorizing()).unwrap_or(false) || shared.recategorizing.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
    }
}

fn poke(workers: &HashMap<String, Worker>, account: &str, job: Job) {
    if let Some(w) = workers.get(account) {
        match job {
            Job::Bodies { .. } | Job::SyncMailbox(_) => {
                let _ = w.now.send(job);
            }
            Job::Sync | Job::Folders => {
                let _ = w.jobs.send(job);
                w.notify.notify_one();
            }
        }
    }
}

/// Wakes an account's worker to plan its wait again (an op was queued or brought forward),
/// without the inbox check a [`Job::Sync`] brings.
fn wake(workers: &HashMap<String, Worker>, account: &str) {
    if let Some(w) = workers.get(account) {
        w.notify.notify_one();
    }
}

async fn handle(shared: &Arc<Shared>, workers: &mut HashMap<String, Worker>, cmd: Command) -> Result<()> {
    match cmd {
        Command::List { req, view, query, conversations, limit, stream, category, counts } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let me = shared.me();
                let q = Query { tab: category, ..Query::parse(&query) };
                let items = shared.reader.lock().map_err(|_| anyhow!("store poisoned")).and_then(|st| {
                    // The top of a long list goes out first, and the rest when it's read. A
                    // Drafts list goes in one piece: its rows are merged with the local drafts.
                    let mut first = |items| shared.emit(Event::List { req, items, more: true });
                    let page = if stream && !drafts::lists_drafts(&st, &view) { FIRST_PAGE } else { 0 };
                    let items = st.list_paged(&view, &q, conversations, limit, &me, page, &mut first)?;
                    // The tabs' counts, after the first page (it never waits for them) and
                    // before the whole list: the app shows both in the same frame.
                    if counts {
                        let counts = st.category_counts(&view, conversations)?;
                        shared.emit(Event::CategoryCounts { req, view: view.clone(), counts });
                    }
                    drafts::merge(&st, &view, &q, limit, items, |a| shared.sender(a))
                });
                match items {
                    Ok(items) => shared.emit(Event::List { req, items, more: false }),
                    Err(e) => shared.emit(Event::Error(format!("search failed: {e}"))),
                }
            });
        }
        Command::Open { req, id, conversations } => {
            let (messages, missing) = {
                let st = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?;
                let messages = st.thread(id, conversations)?;
                // Ask for any bodies we don't have, grouped by mailbox, each with the generation
                // its UIDs are from: they're stored only while the cache is still in it.
                let mut missing: HashMap<(String, String), Vec<u32>> = HashMap::new();
                for m in messages.iter().filter(|m| m.body.is_none()) {
                    missing.entry((m.summary.account.clone(), m.summary.mailbox.clone())).or_default().push(m.summary.uid);
                }
                let missing = missing.into_iter().map(|((account, mailbox), uids)| Ok((st.uidvalidity(&account, &mailbox)?, account, mailbox, uids))).collect::<Result<Vec<_>>>()?;
                (messages, missing)
            };
            shared.emit(Event::Thread { req, id, messages });
            for (validity, account, mailbox, uids) in missing {
                poke(workers, &account, Job::Bodies { mailbox, validity, uids, reply: Some(Reply::Thread(req, id, conversations)) });
            }
            // Why it's in its tab: the writer has the rules. Off the loop, after it shows.
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(Some((category, why))) = shared.with_writer(|st| st.explain(id)) {
                    shared.emit(Event::Explained { id, category, why });
                }
            });
        }
        Command::Prefetch { id, conversations } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let messages = shared.reader.lock().map_err(|_| anyhow!("store poisoned")).and_then(|st| st.thread(id, conversations));
                if let Ok(messages) = messages
                    && !messages.is_empty()
                {
                    shared.emit(Event::Prefetched { id, messages });
                }
            });
        }
        Command::SetFlags { ids, add, remove, thread } => {
            let touched = set_flags(shared, ids, add, remove, thread)?;
            for a in touched {
                shared.emit_mailboxes(&a);
                shared.emit(Event::Changed { account: a.clone() });
                poke(workers, &a, Job::Sync);
            }
            shared.emit_pending();
        }
        Command::Move { ids, to, thread, undo } => {
            let touched = move_and_hold(shared, ids, &to, thread, undo, now())?;
            for a in touched {
                shared.emit_mailboxes(&a);
                shared.emit(Event::Changed { account: a.clone() });
                // Wakes the worker: it replays at once, or re-plans its wait around the hold.
                poke(workers, &a, Job::Sync);
            }
            shared.emit_pending();
        }
        Command::Copy { ids, mailbox, thread, undo } => {
            // Nothing changes here: the copies show once their mailbox is synced.
            for a in copy_and_hold(shared, ids, &mailbox, thread, undo, now())? {
                wake(workers, &a);
            }
            shared.emit_pending();
        }
        Command::Snooze { ids, until, thread, undo } => {
            for a in snooze_and_hold(shared, ids, until, thread, undo, now())? {
                shared.emit_mailboxes(&a);
                shared.emit(Event::Changed { account: a.clone() });
                poke(workers, &a, Job::Sync);
            }
            shared.emit_pending();
        }
        Command::UndoMove { undo } => undo_move(shared, undo)?,
        Command::MoveToCategory { ids, category, undo } => {
            let moved = shared.with_writer(|st| st.move_to_category(&ids, category))?;
            if let Ok(mut u) = shared.category_undo.lock() {
                u.insert(undo, moved);
            }
            shared.emit(Event::Changed { account: String::new() });
        }
        Command::UndoCategory { undo } => {
            if let Some(moved) = shared.category_undo.lock().ok().and_then(|mut u| u.remove(&undo)) {
                shared.with_writer(|st| st.restore_categories(&moved))?;
                shared.emit(Event::Changed { account: String::new() });
            }
        }
        Command::Send { draft, draft_id } => {
            if let Err(e) = queue_send(shared, workers, &draft, draft_id) {
                // Not sent: kept as a draft here, and back in the composer, so nothing written is
                // lost, with why, and its files as the store has them (what's wrong may be them).
                let Some(id) = draft_id else { return Err(e) };
                let kept = shared.with_writer(|st| {
                    st.draft_unsent(id, &draft, None, now())?;
                    shared.emit(Event::SendFailed { draft_id, error: format!("{e:#}") });
                    shared.emit(Event::DraftFiles { draft: id, files: st.files_of(id)? });
                    Ok(())
                });
                if let Err(e2) = kept {
                    log::warn!("couldn't keep an unsent draft: {e2:#}");
                    shared.emit(Event::SendFailed { draft_id, error: format!("{e:#}") });
                }
            }
        }
        Command::UndoSend { op } => {
            // The draft comes back in the composer, the same one: Drafts doesn't get a second.
            let back = shared.with_writer(|st| {
                let (cancelled, back) = st.transaction(|st| {
                    let cancelled = st.cancel_send(op)?;
                    Ok((cancelled, if cancelled { st.unsend_draft(op, now())? } else { None }))
                })?;
                shared.emit(Event::Toast(if cancelled { "Sending undone".into() } else { "Too late — it was already sent".into() }));
                // Sent as read, the store held (see [`open_draft`]).
                Ok(back.map(|d| {
                    let account = d.account.clone();
                    shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: false, notice: None });
                    account
                }))
            })?;
            if let Some(account) = back {
                shared.emit(Event::Changed { account: account.clone() });
                wake(workers, &account);
            }
            shared.emit_pending();
        }
        Command::SaveDraft { id, draft, open, rev, lifecycle } => save_draft(shared, workers, id, &draft, open, DraftRev { lifecycle, rev }, None),
        Command::SaveDraftFrom { id, draft, open, rev, lifecycle, files_from } => save_draft(shared, workers, id, &draft, open, DraftRev { lifecycle, rev }, Some(files_from)),
        Command::AttachFiles { draft, files } => {
            let listed: Vec<_> = files
                .iter()
                .map(|(id, path)| {
                    let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
                    let mime = crate::attach::type_for(&filename).to_owned();
                    let file = crate::model::DraftAttachment { id: *id, filename, mime, size: 0, status: FileStatus::Attaching };
                    (file, FileSource::Path(path.clone()))
                })
                .collect();
            let Some(files) = start_attaching(shared, draft, listed) else { return Ok(()) };
            let max = shared.with_writer(|st| st.draft(draft)).ok().flatten().map(|d| shared.limit(&d.account).max_file()).unwrap_or(crate::attach::LIMIT);
            let shared = shared.clone();
            // Off the engine's loop too: a big file takes a moment to read.
            tokio::task::spawn_blocking(move || {
                for (file, source) in files {
                    if let FileSource::Path(path) = source {
                        read_file_in(&shared, file.id, &path, max);
                    }
                }
            });
        }
        Command::AttachForwarded { draft, message, files } => {
            let listed = files.iter().map(|(f, index)| (crate::model::DraftAttachment { status: FileStatus::Attaching, ..f.clone() }, FileSource::Message { id: message, index: *index })).collect();
            let Some(files) = start_attaching(shared, draft, listed) else { return Ok(()) };
            let parts = files.iter().filter_map(|(f, source)| if let FileSource::Message { index, .. } = source { Some((f.id, *index)) } else { None }).collect();
            forward_files(shared, workers, message, parts)?;
        }
        Command::DetachFile { draft, file } => {
            if shared.with_writer(|st| st.remove_file(draft, file, now()))? {
                if let Some(d) = shared.with_writer(|st| st.draft(draft))? {
                    wake(workers, &d.account);
                }
                shared.emit_pending();
            }
        }
        Command::OpenDraft { id } => match open_draft(shared, id, now())? {
            // Sent by open_draft.
            Opening::Ready(..) | Opening::Copy(..) => {}
            Opening::Download { account, mailbox, validity, uid } => poke(workers, &account, Job::Bodies { mailbox, validity, uids: vec![uid], reply: Some(Reply::Draft(id)) }),
            // Sent or discarded meanwhile: the list catches up.
            Opening::Gone => shared.emit(Event::Changed { account: String::new() }),
        },
        Command::DiscardDraft { id, undo } => {
            discard_draft(shared, workers, id, undo)?;
        }
        Command::DiscardDraftRow { row, undo } => {
            let local = shared.with_writer(|st| {
                Ok(match drafts::draft_of_list_id(row) {
                    Some(d) => st.draft(d)?,
                    None => st.draft_of_message(row)?,
                })
            })?;
            match local {
                Some(d) if discard_draft(shared, workers, d.id, undo)? => shared.emit(Event::DraftDiscarded { id: d.id, undo }),
                // A draft only the server has (written elsewhere): to Trash, like a message.
                None if row > 0 => {
                    for a in move_and_hold(shared, vec![row], &MoveTarget::Trash, false, undo, now())? {
                        shared.emit_mailboxes(&a);
                        shared.emit(Event::Changed { account: a.clone() });
                        poke(workers, &a, Job::Sync);
                    }
                    shared.emit_pending();
                }
                // Already on its way out, or gone: nothing to undo, and the list catches up.
                _ => match undo {
                    Some(key) => shared.emit(Event::Held { undo: key, ops: 0 }),
                    None => shared.emit(Event::Changed { account: String::new() }),
                },
            }
        }
        Command::SyncNow => {
            for (id, w) in workers.iter() {
                let _ = w.jobs.send(Job::Folders);
                w.notify.notify_one();
                shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing("Checking".into()) });
            }
        }
        Command::Watch { view } => {
            let targets: Vec<(String, String)> = shared.with_writer(|st| {
                let accts: Vec<String> = shared.accounts.read().map(|a| a.iter().map(|a| a.id.clone()).collect()).unwrap_or_default();
                Ok(match &view {
                    View::Mailbox { account, mailbox } => vec![(account.clone(), mailbox.clone())],
                    // Every bucket, of every account in the view.
                    View::Role { account, role: Role::Snoozed } => {
                        let mut boxes = Vec::new();
                        for a in accts.iter().filter(|a| account.as_ref().is_none_or(|x| x == *a)) {
                            boxes.extend(st.mailboxes(a)?.into_iter().filter(|m| m.role == Role::Snoozed && m.selectable).map(|m| (a.clone(), m.name)));
                        }
                        boxes
                    }
                    View::Role { account, role } => accts
                        .iter()
                        .filter(|a| account.as_ref().is_none_or(|x| x == *a))
                        .filter_map(|a| st.mailbox_for_role(a, *role).ok().flatten().map(|m| (a.clone(), m)))
                        .collect(),
                    View::Starred { .. } => vec![],
                })
            })?;
            if let Ok(mut w) = shared.watched.lock() {
                w.clear();
                w.extend(targets.iter().cloned());
            }
            for (account, mailbox) in targets {
                // A mailbox IDLE doesn't cover: the worker plans its wait again, to look at it
                // in a while (see session_loop).
                let looked_at = shared.with_writer(|st| Ok(st.role_of(&account, &mailbox) != Role::Inbox)).unwrap_or(false);
                poke(workers, &account, Job::SyncMailbox(mailbox));
                if looked_at {
                    wake(workers, &account);
                }
            }
        }
        Command::AddAccount { account, secret } => {
            let shared2 = shared.clone();
            // Test the login before saving anything.
            let cred = match account.auth {
                AuthKind::Password => Credential::Password(secret.clone()),
                AuthKind::GoogleOAuth => {
                    let (cid, csec) = client_for(&account);
                    let s = secret.clone();
                    match tokio::task::spawn_blocking(move || oauth::refresh(&cid, &csec, &s)).await? {
                        Ok(t) => Credential::OAuth(t.access_token),
                        Err(e) => {
                            shared2.emit(Event::AccountFailed(format!("{e:#}")));
                            return Ok(());
                        }
                    }
                }
            };
            match imap::connect(&account, &cred).await {
                Ok(mut session) => {
                    let _ = session.logout().await;
                }
                Err(e) => {
                    shared.emit(Event::AccountFailed(format!("{e:#}")));
                    return Ok(());
                }
            }
            secrets::set(&account.id, &secret)?;
            let list = {
                let mut accts = shared.accounts.write().map_err(|_| anyhow!("poisoned"))?;
                accts.retain(|a| a.id != account.id);
                accts.push(account.clone());
                accounts::save(&accts)?;
                accts.clone()
            };
            shared.store_me();
            shared.emit(Event::Accounts(list));
            shared.emit(Event::AccountAdded(account.clone()));
            // Its address is yours: mail to it is to you.
            update_classifier(shared);
            workers.insert(account.id.clone(), spawn_worker(shared.clone(), account.id.clone()));
        }
        Command::RemoveAccount { id } => {
            if let Some(w) = workers.remove(&id) {
                w.task.abort();
                w.fg_task.abort();
            }
            if let Ok(mut w) = shared.wakers.lock() {
                w.remove(&id);
            }
            secrets::remove(&id);
            let list = {
                let mut accts = shared.accounts.write().map_err(|_| anyhow!("poisoned"))?;
                accts.retain(|a| a.id != id);
                accounts::save(&accts)?;
                accts.clone()
            };
            shared.store_me();
            shared.with_writer(|st| st.forget_account(&id))?;
            update_classifier(shared);
            shared.emit(Event::Accounts(list));
            shared.emit(Event::Changed { account: id });
        }
        Command::GoogleSignIn { client_id, client_secret, hint } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let r = oauth::authorize(&client_id, &client_secret, Some(&hint), |url| {
                    if let Err(e) = open_browser(url) {
                        log::warn!("couldn't open the browser: {e}");
                    }
                });
                match r {
                    Ok(t) => match t.refresh_token {
                        Some(rt) => shared.emit(Event::GoogleSignedIn { email: t.email.unwrap_or(hint), name: t.name.unwrap_or_default(), refresh_token: rt }),
                        None => shared.emit(Event::GoogleFailed("Google didn't return a refresh token; try again.".into())),
                    },
                    Err(e) => shared.emit(Event::GoogleFailed(format!("{e:#}"))),
                }
            });
        }
        Command::SaveAttachment { id, index } => {
            let raw = shared.with_writer(|st| st.raw(id))?.ok_or_else(|| anyhow!("The message isn't downloaded yet"))?;
            let dir = directories::UserDirs::new().and_then(|d| d.download_dir().map(|p| p.to_path_buf())).unwrap_or_else(std::env::temp_dir);
            let path = crate::mime::save_attachment(&raw, index, &dir)?;
            shared.emit(Event::Saved(path));
        }
        Command::OpenOriginal { id } => {
            // The message as received, for the system's mail viewer: nothing in it is rendered
            // by a browser that ignores Envoy's image setting.
            let raw = shared.with_writer(|st| st.raw(id))?.ok_or_else(|| anyhow!("The message isn't downloaded yet"))?;
            export_in_background(shared, format!("message-{id}"), "eml", move || raw);
        }
        Command::OpenInBrowser { id, remote_images } => {
            let (body, raw) = shared.with_writer(|st| Ok((st.body(id)?, st.raw(id)?)))?;
            let html = body.and_then(|b| b.html).ok_or_else(|| anyhow!("This message has no HTML to open"))?;
            export_in_background(shared, format!("message-{id}-safe"), "html", move || crate::mime::browser_html(&html, raw.as_deref(), remote_images).into_bytes());
        }
        Command::FetchImage { url, message } => {
            let shared = shared.clone();
            let raw = if url.starts_with("cid:") { shared.with_writer(|st| st.raw(message)).ok().flatten() } else { None };
            tokio::spawn(async move {
                let permit = IMAGE_SLOTS.acquire().await;
                let u = url.clone();
                let r = tokio::task::spawn_blocking(move || fetch_image(&u, raw.as_deref())).await;
                drop(permit);
                match r {
                    Ok(Ok((width, height, rgba))) => shared.emit(Event::Image { url, width, height, rgba }),
                    Ok(Err(e)) => {
                        log::debug!("image {url}: {e:#}");
                        shared.emit(Event::ImageFailed { url });
                    }
                    Err(_) => shared.emit(Event::ImageFailed { url }),
                }
            });
        }
        Command::Settings(s) => {
            if let Ok(mut cur) = shared.settings.write() {
                *cur = *s;
            }
            update_classifier(shared);
            for w in workers.values() {
                w.notify.notify_one();
            }
        }
        Command::Contacts { req, typed } => {
            // Off the engine's loop: a list being read holds the reader a moment.
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let me = shared.me();
                match shared.reader.lock().map_err(|_| anyhow!("store poisoned")).and_then(|st| st.suggest(&typed, &me, now(), SUGGESTIONS)) {
                    Ok(contacts) => shared.emit(Event::Contacts { req, contacts }),
                    Err(e) => log::warn!("recipient suggestions: {e:#}"),
                }
            });
        }
    }
    Ok(())
}

/// Makes an export's bytes and writes them ([`crate::paths::write_export`]) on a blocking
/// thread: sanitizing parses the message, and on Linux choosing the folder asks `xdg-mime` for
/// the default app. Emits [`Event::Saved`], or [`Event::Error`].
fn export_in_background(shared: &Arc<Shared>, stem: String, ext: &'static str, bytes: impl FnOnce() -> Vec<u8> + Send + 'static) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || match crate::paths::write_export(&stem, ext, &bytes()) {
        Ok(path) => shared.emit(Event::Saved(path)),
        Err(e) => shared.emit(Event::Error(format!("Couldn't write the file to open: {e}"))),
    });
}

/// [`Command::SaveDraft`] and [`Command::SaveDraftFrom`]: saves, then says how it went.
fn save_draft(shared: &Shared, workers: &HashMap<String, Worker>, id: i64, draft: &Draft, open: bool, at: DraftRev, files_from: Option<i64>) {
    let saved = shared.with_writer(|st| {
        let saved = drafts::save_from(st, id, draft, open, at, now(), files_from)?;
        // Carried on from another draft: its files, as they are now (one may have come in, or
        // been lost, under the old id), the store held so nothing comes between.
        if saved.is_some() && files_from.is_some() {
            shared.emit(Event::DraftFiles { draft: id, files: st.files_of(id)? });
        }
        Ok(saved)
    });
    match saved {
        Ok(Some(queued)) => {
            // Only now is it saved: the app shows "Draft saved", and drops its own copy.
            shared.emit(Event::DraftSaved { id, rev: at.rev });
            if queued {
                shared.emit_pending();
                wake(workers, &draft.account);
            }
        }
        Ok(None) => shared.emit(Event::DraftSaveRefused { id, rev: at.rev }),
        Err(e) => shared.emit(Event::DraftSaveFailed { id, rev: at.rev, error: format!("{e:#}") }),
    }
}

/// Lists `files` with draft `draft`, as being attached ([`Store::attaching`]). Those it couldn't
/// (no such draft, or an id it already has) are answered [`Event::AttachFailed`] here; the rest
/// are returned, to be read in.
fn start_attaching(shared: &Shared, draft: i64, files: Vec<(crate::model::DraftAttachment, FileSource)>) -> Option<Vec<(crate::model::DraftAttachment, FileSource)>> {
    let taken = match shared.with_writer(|st| st.attaching(draft, &files)) {
        Ok(Some(taken)) => taken,
        Ok(None) => {
            for (f, _) in &files {
                shared.emit(Event::AttachFailed { draft, file: f.id, error: format!("Couldn't attach {}: the draft isn't saved", f.filename) });
            }
            return None;
        }
        Err(e) => {
            for (f, _) in &files {
                shared.emit(Event::AttachFailed { draft, file: f.id, error: format!("Couldn't attach {} ({e:#})", f.filename) });
            }
            return None;
        }
    };
    let mut go = Vec::new();
    for ((f, source), taken) in files.into_iter().zip(taken) {
        if taken {
            go.push((f, source));
        } else {
            // Never another file's id: what's there stays as it is.
            shared.emit(Event::AttachFailed { draft, file: f.id, error: format!("Couldn't attach {}: attach it again", f.filename) });
        }
    }
    Some(go)
}

/// Reads file `file` from `path` into the drafts attaching it (at most `max` bytes).
fn read_file_in(shared: &Shared, file: i64, path: &std::path::Path, max: usize) {
    match crate::attach::read_file(path, file, max) {
        Ok((f, data)) => file_in(shared, file, &data, &f.mime),
        Err(error) => file_failed(shared, file, &error),
    }
}

/// File `file`'s bytes are read ([`Store::file_in`]): the drafts attaching it hear, and their
/// uploads are planned.
/// File events are sent with the store held, as the change is made: in the order the changes
/// happened, whatever thread made them.
fn file_in(shared: &Shared, file: i64, data: &[u8], mime: &str) {
    let done = shared.with_writer(|st| {
        for (draft, f) in st.file_in(file, data, mime, now())? {
            if let Some(d) = st.draft(draft)? {
                shared.wake_account(&d.account);
            }
            shared.emit(Event::FileAttached { draft, file: f });
        }
        Ok(())
    });
    match done {
        Ok(()) => shared.emit_pending(),
        Err(e) => file_failed(shared, file, &format!("Couldn't attach it ({e:#})")),
    }
}

fn file_failed(shared: &Shared, file: i64, error: &str) {
    let _ = shared.with_writer(|st| {
        for draft in st.file_failed(file, error)? {
            shared.emit(Event::AttachFailed { draft, file, error: error.to_owned() });
        }
        Ok(())
    });
}

/// Reads `(file id, index)` attachments of message `message` into the drafts attaching them:
/// from the cache, or once the message is downloaded, on the connection that serves what the
/// user is waiting on.
fn forward_files(shared: &Arc<Shared>, workers: &HashMap<String, Worker>, message: MessageId, files: Vec<(i64, usize)>) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    // With the generation of its mailbox its UID is from: the body is stored only in it.
    let (summary, here) = shared.with_writer(|st| {
        let summary = match st.summary(message)? {
            Some(s) => Some((st.uidvalidity(&s.account, &s.mailbox)?, s)),
            None => None,
        };
        Ok((summary, st.has_raw(message)?))
    })?;
    match summary {
        _ if here => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || attach_forwarded(&shared, message, &files));
        }
        Some((validity, s)) if workers.contains_key(&s.account) => {
            poke(workers, &s.account, Job::Bodies { mailbox: s.mailbox, validity, uids: vec![s.uid], reply: Some(Reply::Attach { message, files }) });
        }
        _ => {
            for (file, _) in files {
                file_failed(shared, file, "The original message is no longer on this computer");
            }
        }
    }
    Ok(())
}

/// [`forward_files`] once the message is on this computer: its files into the drafts.
fn attach_forwarded(shared: &Shared, message: MessageId, files: &[(i64, usize)]) {
    let found = match shared.with_writer(|st| st.raw(message)) {
        Ok(Some(raw)) => crate::mime::forwarded_files(&raw, files),
        Ok(None) => files.iter().map(|_| Err("The original message is no longer on this computer".to_owned())).collect(),
        Err(e) => files.iter().map(|_| Err(format!("Couldn't read the original message ({e:#})"))).collect(),
    };
    for (&(file, _), found) in files.iter().zip(found) {
        match found {
            Ok((f, data)) => file_in(shared, file, &data, &f.mime),
            Err(error) => file_failed(shared, file, &error),
        }
    }
}

/// Files a restart finds still being attached (cut off by a quit or a crash) are taken up
/// again: read from where they came from, or, if that's gone, said so on their chip.
fn resume_attaching(shared: &Arc<Shared>, workers: &HashMap<String, Worker>) {
    let pending = match shared.with_writer(|st| st.attaching_files()) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("couldn't read the files being attached: {e:#}");
            return;
        }
    };
    let mut forwards: HashMap<MessageId, Vec<(i64, usize)>> = HashMap::new();
    let mut paths = Vec::new();
    for (draft, file, source) in pending {
        match source {
            FileSource::Path(path) => {
                let max = shared.with_writer(|st| st.draft(draft)).ok().flatten().map(|d| shared.limit(&d.account).max_file()).unwrap_or(crate::attach::LIMIT);
                paths.push((file, path, max));
            }
            FileSource::Message { id, index } => forwards.entry(id).or_default().push((file, index)),
        }
    }
    if !paths.is_empty() {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || {
            for (file, path, max) in paths {
                read_file_in(&shared, file, &path, max);
            }
        });
    }
    for (message, files) in forwards {
        if let Err(e) = forward_files(shared, workers, message, files) {
            log::warn!("couldn't take up a forward's files: {e:#}");
        }
    }
}

/// [`Command::DiscardDraft`]: hides the draft and queues its purge, held for the undo window
/// like a move (with its second of grace, see [`move_and_hold`]). False if there was nothing to
/// discard.
fn discard_draft(shared: &Shared, workers: &HashMap<String, Worker>, id: i64, undo: Option<i64>) -> Result<bool> {
    let window = shared.setting_u32("behavior.undoSeconds");
    let hold = undo.filter(|_| window > 0).map(|key| (now() + window as i64 + 1, key));
    let Some(account) = shared.with_writer(|st| st.discard_draft(id, hold))? else { return Ok(false) };
    if let Some((_, key)) = hold
        && let Ok(mut keys) = shared.undo_keys.lock()
    {
        *keys.entry(key).or_insert(0) += 1;
    }
    shared.emit(Event::Changed { account: account.clone() });
    shared.emit_pending();
    wake(workers, &account);
    Ok(true)
}

/// [`Command::Send`]: builds the message and queues it for `compose.undoSendSeconds`, with the
/// composer's draft waiting alongside it (hidden from Drafts) until it's out.
fn queue_send(shared: &Arc<Shared>, workers: &HashMap<String, Worker>, draft: &Draft, draft_id: Option<i64>) -> Result<()> {
    let account = shared.account(&draft.account).ok_or_else(|| anyhow!("Choose an account to send from"))?;
    // The draft's files, as the store has them, read at once with their bytes.
    let (listed, files) = match draft_id {
        // One snapshot: another process can't change them between the two reads.
        Some(d) => shared.with_writer(|st| st.snapshot(|st| Ok((st.files_of(d)?, st.attached_files(d)?))))?,
        None if draft.attachments.is_empty() => (Vec::new(), Vec::new()),
        None => return Err(anyhow!("The attached files aren't saved with this message")),
    };
    // What goes is what the composer shows: the same files, or nothing goes (another window
    // changed them, or they were lost), and the composer is told what's there.
    let shown: HashSet<i64> = draft.attachments.iter().map(|f| f.id).collect();
    let stored: HashSet<i64> = listed.iter().map(|f| f.id).collect();
    if shown != stored {
        let names = |files: &[crate::model::DraftAttachment], other: &HashSet<i64>| files.iter().filter(|f| !other.contains(&f.id)).map(|f| f.filename.as_str()).collect::<Vec<_>>().join(", ");
        let (added, gone) = (names(&listed, &shown), names(&draft.attachments, &stored));
        let what = match (added.is_empty(), gone.is_empty()) {
            (false, true) => format!("added: {added}"),
            (true, false) => format!("gone: {gone}"),
            _ => format!("added: {added}; gone: {gone}"),
        };
        return Err(anyhow!("Not sent: its attachments changed ({what}). Check them and send again."));
    }
    if let Some(f) = listed.iter().find(|f| f.status == FileStatus::Attaching) {
        return Err(anyhow!("Still attaching {}…", f.filename));
    }
    if let Some(f) = listed.iter().find(|f| matches!(f.status, FileStatus::Failed(_))) {
        return Err(anyhow!("{} isn't attached: remove it, or attach it again", f.filename));
    }
    // Refused here, not bounced by the server later: first as the composer judges it, then
    // the message itself, as built.
    let limit = shared.limit(&account.id);
    if let Some(why) = limit.over(&listed.iter().map(|f| f.size).collect::<Vec<_>>(), draft.body.len() + draft.subject.len()) {
        return Err(anyhow!(why));
    }
    let built = smtp::build(&account, draft, files)?;
    if let Some(why) = limit.refuses(built.raw.len()) {
        return Err(anyhow!(why));
    }
    let undo = shared.setting_u32("compose.undoSendSeconds");
    let raw_b64 = base64::engine::general_purpose::STANDARD.encode(&built.raw);
    // The draft waits with the message (hidden from Drafts) and goes once it's out. The message
    // replied to is pinned down in the same transaction, as it is now: its own account (which
    // may not be the one sending), mailbox, UID, generation and Message-ID.
    let id = shared.with_writer(|st| {
        st.transaction(|st| {
            let sent_mailbox = if account.is_gmail() { None } else { st.mailbox_for_role(&account.id, Role::Sent)? };
            let reply_of = match draft.reply_of {
                Some(id) => st.original(id)?.map(ReplyOf::Original),
                None => None,
            };
            let op = Op::Send { raw_b64, from: built.from, to: built.to, sent_mailbox, reply_of };
            let id = st.enqueue_at(&account.id, &op, now() + undo as i64)?;
            if let Some(d) = draft_id {
                st.draft_sending(d, draft, id, now())?;
            }
            Ok(id)
        })
    })?;
    shared.emit(Event::Sent { op: id, undo_secs: undo, draft_id });
    if draft_id.is_some() {
        shared.emit(Event::Changed { account: account.id.clone() });
    }
    shared.emit_pending();
    poke(workers, &account.id, Job::Sync);
    Ok(())
}

/// [`Command::SetFlags`]'s local part: changes the flags, adjusts unread counts and queues the
/// change. Returns the accounts touched.
fn set_flags(shared: &Shared, ids: Vec<MessageId>, add: u32, remove: u32, thread: bool) -> Result<HashSet<String>> {
    let mut touched = HashSet::new();
    // Messages hidden for a move aren't on screen and are about to leave: a late Undo can
    // reopen one for a moment, but it must not be marked read (or counted down) meanwhile.
    // Expansion only adds visible messages.
    let ids = shared.with_writer(|st| Ok(ids.into_iter().filter(|id| st.movable(*id).ok().flatten().is_some()).collect()))?;
    let ids = expand(shared, ids, thread, false)?;
    shared.with_writer(|st| {
        for id in &ids {
            let before = st.summary(*id)?.map(|s| s.flags).unwrap_or(0);
            if remove & flags::WOKEN != 0 {
                st.forget_woken(*id)?;
            }
            if let Some((account, mailbox, uid)) = st.change_flags(*id, add, remove)? {
                let after = (before | add) & !remove;
                if after == before {
                    continue;
                }
                if (before ^ after) & flags::SEEN != 0 {
                    st.bump_unread(&account, &mailbox, if after & flags::SEEN != 0 { -1 } else { 1 })?;
                }
                st.enqueue(&account, &Op::SetFlags { mailbox, uid, add: add & !before, remove: remove & before })?;
                touched.insert(account);
            }
        }
        Ok(())
    })?;
    Ok(touched)
}

/// [`Command::Move`]'s local part: applies the move, queues it (held for the undo window when
/// `undo` is set) and remembers the undo key. Returns the accounts touched.
fn move_and_hold(shared: &Shared, ids: Vec<MessageId>, to: &MoveTarget, thread: bool, undo: Option<i64>, now: i64) -> Result<HashSet<String>> {
    let ids = expand(shared, ids, thread, true)?;
    let hold = hold_for(shared, undo, now);
    let moved = shared.with_writer(|st| st.transaction(|st| move_local(st, &ids, to, hold)))?;
    Ok(held(shared, hold, moved))
}

/// [`Command::Snooze`]'s local part: [`move_and_hold`], each message to its account's bucket
/// for `until`.
fn snooze_and_hold(shared: &Shared, ids: Vec<MessageId>, until: i64, thread: bool, undo: Option<i64>, now: i64) -> Result<HashSet<String>> {
    let ids = expand(shared, ids, thread, true)?;
    let hold = hold_for(shared, undo, now);
    let moved = shared.with_writer(|st| {
        st.transaction(|st| {
            let mut by_account: Vec<(String, Vec<MessageId>)> = Vec::new();
            for id in &ids {
                let Some((account, _, _)) = st.movable(*id)? else { continue };
                match by_account.iter_mut().find(|(a, _)| *a == account) {
                    Some((_, ids)) => ids.push(*id),
                    None => by_account.push((account, vec![*id])),
                }
            }
            let mut all = Moved::default();
            for (account, ids) in by_account {
                let bucket = st.snooze_bucket(&account, until)?;
                all.add(move_local(st, &ids, &MoveTarget::Mailbox(bucket), hold)?);
            }
            Ok(all)
        })
    })?;
    Ok(held(shared, hold, moved))
}

/// When an undoable action's ops may reach the server, with its undo key: the window, and a
/// second of grace on top, so the toast's Undo button (shown for exactly the window) never
/// outlives the hold, whatever the clocks' rounding. None: at once.
fn hold_for(shared: &Shared, undo: Option<i64>, now: i64) -> Option<(i64, i64)> {
    let window = shared.setting_u32("behavior.undoSeconds");
    undo.filter(|_| window > 0).map(|key| (now + window as i64 + 1, key))
}

/// Tells the UI what a move held, remembers its undo key, and shows its problems. Returns the
/// accounts it touched.
fn held(shared: &Shared, hold: Option<(i64, i64)>, mut moved: Moved) -> HashSet<String> {
    match hold {
        Some((_, key)) => shared.emit(Event::Held { undo: key, ops: moved.held }),
        // Without an undo key nobody hears about a move that did nothing; the UI took its
        // rows away optimistically, so tell it to reload.
        None if moved.touched.is_empty() => shared.emit(Event::Changed { account: String::new() }),
        None => {}
    }
    if let Some((_, key)) = hold
        && moved.held > 0
        && let Ok(mut keys) = shared.undo_keys.lock()
    {
        // Only the last few can still be undone; keep the map small.
        if keys.len() >= 256 {
            keys.clear();
        }
        keys.insert(key, moved.held);
    }
    moved.problems.dedup();
    for p in moved.problems {
        shared.emit(Event::Error(p));
    }
    moved.touched
}

/// [`Command::Copy`]'s local part: queues a copy of each message (each conversation's messages
/// in its mailbox, with `thread`) to `dest`, held for the undo window when `undo` is set, and
/// remembers the undo key. Messages already in `dest`, or on their way out, are left alone.
/// Returns the accounts touched.
fn copy_and_hold(shared: &Shared, ids: Vec<MessageId>, dest: &str, thread: bool, undo: Option<i64>, now: i64) -> Result<HashSet<String>> {
    let ids = expand(shared, ids, thread, true)?;
    let window = shared.setting_u32("behavior.undoSeconds");
    // The second of grace, as for moves (move_and_hold).
    let hold = undo.filter(|_| window > 0).map(|key| (now + window as i64 + 1, key));
    let (touched, held) = shared.with_writer(|st| {
        st.transaction(|st| {
            let mut touched = HashSet::new();
            let mut held = 0;
            for id in &ids {
                let Some((account, mailbox, uid)) = st.movable(*id)? else { continue };
                if mailbox == dest {
                    continue;
                }
                let op = Op::Copy { mailbox, uid, dest: dest.to_owned() };
                match hold {
                    Some((not_before, key)) => {
                        st.enqueue_held(&account, &op, not_before, key)?;
                        held += 1;
                    }
                    None => {
                        st.enqueue(&account, &op)?;
                    }
                }
                touched.insert(account);
            }
            Ok((touched, held))
        })
    })?;
    if let Some((_, key)) = hold {
        shared.emit(Event::Held { undo: key, ops: held });
        if held > 0
            && let Ok(mut keys) = shared.undo_keys.lock()
        {
            if keys.len() >= 256 {
                keys.clear();
            }
            keys.insert(key, held);
        }
    }
    Ok(touched)
}

/// [`Command::UndoMove`]: takes back what's still queued of a move and tells the UI how it went.
///
/// Only ops that haven't been claimed for replay are cancelled. One that is on the wire, or
/// already done, can't be called back: it's reported as too late and left to finish, and the
/// reload shows where its messages really are (the UI put the rows back optimistically).
/// Undo never queues a reverse move: Envoy doesn't learn a moved message's new UID, and a
/// toast's Undo can only race a replay in the second of grace after the window.
fn undo_move(shared: &Shared, undo: i64) -> Result<()> {
    let done = shared.with_writer(|st| st.undo_batch(undo))?;
    let queued = shared.undo_keys.lock().ok().and_then(|mut k| k.remove(&undo)).unwrap_or(0);
    // Ops held for this action that weren't cancelled: on the wire, or already replayed.
    let late = queued.saturating_sub(done.released).max(done.in_flight);
    if late > 0 {
        shared.emit(Event::Toast(if done.released == 0 {
            "Too late to undo — it already went to the server".into()
        } else {
            format!("Partly undone — {late} already went to the server")
        }));
    }
    if done.accounts.is_empty() {
        // Nothing came back (too late, or the move had nothing to do, say archiving in All
        // Mail): reload so the list shows where the messages really are.
        shared.emit(Event::Changed { account: String::new() });
    }
    for a in done.accounts {
        shared.emit_mailboxes(&a);
        shared.emit(Event::Changed { account: a });
    }
    shared.emit_pending();
    Ok(())
}

/// Where [`open_draft`] got.
enum Opening {
    Ready(Box<LocalDraft>, Option<DraftNotice>),
    /// A server draft that can't be edited in place (`why`): an unsaved copy to start from.
    Copy(Box<Draft>, String),
    /// A server draft not downloaded yet: fetch it (UID `uid` in generation `validity` of
    /// `mailbox`), then try again.
    Download { account: String, mailbox: String, validity: u32, uid: u32 },
    /// Nothing to open (sent, discarded, or deleted meanwhile).
    Gone,
}

/// [`Command::OpenDraft`]: the local draft a Drafts row stands for. A server copy of a local
/// draft opens that draft (its content may be newer); a draft only the server has (written
/// elsewhere) becomes a local draft, so editing it replaces it there.
fn open_draft(shared: &Shared, id: MessageId, now: i64) -> Result<Opening> {
    shared.with_writer(|st| {
        let local = match drafts::draft_of_list_id(id) {
            Some(d) => st.draft(d)?,
            None => st.draft_of_message(id)?,
        };
        let opening = match local {
            Some(d) if d.visible => Opening::Ready(Box::new(d), None),
            Some(_) => Opening::Gone,
            None if id < 0 => Opening::Gone,
            None => match (st.summary(id)?, st.raw(id)?) {
                (None, _) => Opening::Gone,
                (Some(s), None) => Opening::Download { validity: st.uidvalidity(&s.account, &s.mailbox)?, account: s.account, mailbox: s.mailbox, uid: s.uid },
                (Some(s), Some(raw)) => match crate::mime::server_draft(&raw) {
                    // Left as it is on the server: the copy becomes a draft of its own if edited.
                    Some(crate::mime::ServerDraft { draft, notice: Some(DraftNotice::Copy(why)), .. }) => Opening::Copy(Box::new(Draft { account: s.account, ..draft }), why),
                    Some(found) => st.adopt_draft(id, &found.draft, &found.files, now)?.map_or(Opening::Gone, |d| Opening::Ready(Box::new(d), found.notice)),
                    None => Opening::Gone,
                },
            },
        };
        // Sent as read, the store held meanwhile: a file event (sent the same way) can't come
        // between, so the composer can take what follows as it comes.
        match &opening {
            Opening::Ready(d, notice) => {
                st.set_draft_open(d.id, true)?;
                shared.emit(Event::DraftOpened { id: d.id, draft: d.draft.clone(), at: d.at, restored: false, notice: notice.clone() });
            }
            Opening::Copy(draft, why) => shared.emit(Event::DraftOpened { id: 0, draft: (**draft).clone(), at: DraftRev::default(), restored: false, notice: Some(DraftNotice::Copy(why.clone())) }),
            _ => {}
        }
        Ok(opening)
    })
}

fn expand(shared: &Shared, ids: Vec<MessageId>, thread: bool, same_mailbox: bool) -> Result<Vec<MessageId>> {
    if !thread {
        return Ok(ids);
    }
    shared.with_writer(|st| {
        let mut out = Vec::new();
        for id in ids {
            for t in st.thread_ids(id, same_mailbox)? {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            if !out.contains(&id) {
                out.push(id);
            }
        }
        Ok(out)
    })
}

/// Applies a move locally and queues it for the server. Moved messages are hidden, not
/// deleted, until their op replays ([`Store::op_done`]); with `hold = Some((not_before, key))`
/// the ops wait until `not_before` under undo key `key` ([`Store::undo_batch`]). Deleting
/// from Trash expunges at once and can't be undone. Returns the accounts touched and any
/// problems to show.
fn move_local(st: &Store, ids: &[MessageId], to: &MoveTarget, hold: Option<(i64, i64)>) -> Result<Moved> {
    let mut touched = HashSet::new();
    let mut problems = Vec::new();
    let mut held = 0;
    for id in ids {
        // Gone, or already moving (say, its undo came too late and the UI showed it again
        // before the reload): nothing to do. The caller runs this in one transaction.
        let Some((account, mailbox, uid)) = st.movable(*id)? else { continue };
        let here = st.role_of(&account, &mailbox);
        let dest = match to {
            MoveTarget::Archive => st.mailbox_for_role(&account, Role::Archive)?.or(st.mailbox_for_role(&account, Role::All)?),
            MoveTarget::Trash => st.mailbox_for_role(&account, Role::Trash)?,
            MoveTarget::Junk => st.mailbox_for_role(&account, Role::Junk)?,
            MoveTarget::Inbox => st.mailbox_for_role(&account, Role::Inbox)?,
            MoveTarget::Mailbox(m) => Some(m.clone()),
        };
        let summary = st.summary(*id)?;
        let unread = summary.as_ref().is_some_and(|s| !s.seen());
        if *to == MoveTarget::Trash && here == Role::Trash {
            // Queued first, while the message (its Message-ID) is still here to record.
            st.enqueue(&account, &Op::Expunge { mailbox: mailbox.clone(), uid })?;
            st.remove_local(*id)?;
            st.bump_counts(&account, &mailbox, if unread { -1 } else { 0 }, -1)?;
        } else if *to == MoveTarget::Archive && matches!(here, Role::All | Role::Archive) {
            continue;
        } else {
            let Some(dest) = dest else {
                problems.push(format!("No {} folder found for this account", match to { MoveTarget::Archive => "archive", MoveTarget::Trash => "trash", MoveTarget::Junk => "spam", _ => "destination" }));
                continue;
            };
            if dest == mailbox {
                continue;
            }
            // Dealt with: a message back from a snooze isn't pinned any more, wherever it goes.
            st.forget_woken(*id)?;
            if summary.as_ref().is_some_and(|s| s.flags & flags::WOKEN != 0) {
                st.change_flags(*id, 0, flags::WOKEN)?;
                st.enqueue(&account, &Op::SetFlags { mailbox: mailbox.clone(), uid, add: 0, remove: flags::WOKEN })?;
            }
            let op = Op::Move { mailbox: mailbox.clone(), uid, dest };
            let op_id = match hold {
                Some((not_before, key)) => {
                    held += 1;
                    st.enqueue_held(&account, &op, not_before, key)?
                }
                None => st.enqueue(&account, &op)?,
            };
            // Hidden, with its mailbox's counts adjusted.
            anyhow::ensure!(st.hide_local(*id, op_id)?, "message {id} is already moving");
        }
        touched.insert(account);
    }
    Ok(Moved { touched, problems, held })
}

/// What [`move_local`] did.
#[derive(Default)]
struct Moved {
    touched: HashSet<String>,
    problems: Vec<String>,
    /// Ops queued under the undo key.
    held: usize,
}

impl Moved {
    fn add(&mut self, other: Moved) {
        self.touched.extend(other.touched);
        self.problems.extend(other.problems);
        self.held += other.held;
    }
}

static IMAGE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);

const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;

/// Bytes for an image: a `cid:` part of the message, the disk cache, or the network.
fn image_bytes(url: &str, raw: Option<&[u8]>) -> Result<Vec<u8>> {
    if url.starts_with("cid:") {
        return crate::mime::inline_part(raw.ok_or_else(|| anyhow!("message not downloaded"))?, url).ok_or_else(|| anyhow!("no such inline part"));
    }
    use sha2::Digest;
    let dir = paths::data_dir().join("images");
    let key: String = sha2::Sha256::digest(url.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(key);
    if let Ok(b) = std::fs::read(&path) {
        return Ok(b);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent("Mozilla/5.0 (compatible; Envoy mail)")
        .build()
        .into();
    let mut resp = agent.get(url).call()?;
    let bytes = resp.body_mut().with_config().limit(MAX_IMAGE_BYTES).read_to_vec()?;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &bytes);
    Ok(bytes)
}

pub fn fetch_image(url: &str, raw: Option<&[u8]>) -> Result<(u32, u32, Arc<[u8]>)> {
    let bytes = image_bytes(url, raw)?;
    let img = image::load_from_memory(&bytes)?;
    // Email images are shown at most ~1000 points wide; keep 2× of that.
    let img = if img.width() > 2000 { img.resize(2000, 2000 * img.height() / img.width().max(1), image::imageops::FilterType::Triangle) } else { img };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((w, h, Arc::from(rgba.into_raw())))
}

pub fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    r.map(|_| ())
}

/// Asks the account's SMTP server what it takes (its EHLO `SIZE`), once: what it said is kept,
/// and each send hears it again. Without an answer, the composer's 25 MB rule stands.
async fn learn_server_size(shared: Arc<Shared>, id: String) {
    if shared.with_writer(|st| st.send_limit(&id)).ok().flatten().is_some() {
        return;
    }
    let Some(account) = shared.account(&id) else { return };
    match smtp::server_size(&account).await {
        Ok(size) => shared.learned_size(&id, size),
        Err(e) => log::debug!("{}: couldn't ask the SMTP server what it takes: {e:#}", account.email),
    }
}

fn spawn_worker(shared: Arc<Shared>, account: String) -> Worker {
    let (jobs, rx) = mpsc::unbounded_channel();
    let (now, now_rx) = mpsc::unbounded_channel();
    let notify = Arc::new(Notify::new());
    if let Ok(mut w) = shared.wakers.lock() {
        w.insert(account.clone(), notify.clone());
    }
    tokio::spawn(learn_server_size(shared.clone(), account.clone()));
    let task = tokio::spawn(worker(shared.clone(), account.clone(), rx, notify.clone()));
    let fg_task = tokio::spawn(foreground(shared, account, now_rx, jobs.clone(), notify.clone()));
    Worker { jobs, now, notify, task, fg_task }
}

/// The foreground connection: opened on demand, closed after five idle minutes. If it can't
/// connect, its jobs fall back to the background loop, so nothing is lost.
async fn foreground(shared: Arc<Shared>, id: String, mut rx: mpsc::UnboundedReceiver<Job>, fallback: mpsc::UnboundedSender<Job>, notify: Arc<Notify>) {
    let mut session: Option<Session> = None;
    let mut used = Used::now();
    loop {
        let job = match &session {
            Some(_) => match tokio::time::timeout(Duration::from_secs(300), rx.recv()).await {
                Ok(Some(j)) => j,
                Ok(None) => return,
                Err(_) => {
                    if let Some(mut s) = session.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(5), s.logout()).await;
                    }
                    continue;
                }
            },
            None => match rx.recv().await {
                Some(j) => j,
                None => return,
            },
        };
        let Some(account) = shared.account(&id) else { return };
        // Said as it starts and as it ends: a job that stalls shows, not only one that fails.
        let (what, started, idle) = (job.what(), Instant::now(), used.idle());
        log::debug!("{}: foreground: {what}", account.email);
        match on_foreground(&mut ImapForeground { shared: &shared, account: &account }, &mut session, idle, &job).await {
            Ok(()) => log::debug!("{}: foreground: {what}: done in {:?}", account.email, started.elapsed()),
            Err(e) => {
                log::warn!("{}: a foreground {} failed after {:?}: left to the background connection", account_tag(&account.id), job.kind(), started.elapsed());
                log::debug!("{}: foreground: {what}: {e:#}", account.email);
                let _ = fallback.send(job);
                notify.notify_one();
            }
        }
        used = Used::now();
    }
}

/// A foreground connection left this long since its last job is [`Foreground::probe`]d before
/// the next: long enough that one in use never is, short enough to beat NATs and sleep.
const PROBE_AFTER: Duration = Duration::from_secs(60);

/// When the foreground connection was last used, by both clocks: the monotonic one stands still
/// while the computer sleeps (on macOS and Linux), the wall clock doesn't, so a connection that
/// slept with the computer is probed however briefly it's been awake since.
#[derive(Clone, Copy)]
struct Used {
    at: Instant,
    wall: std::time::SystemTime,
}

impl Used {
    fn now() -> Used {
        Used { at: Instant::now(), wall: std::time::SystemTime::now() }
    }

    /// How long since, by whichever clock says longer (a wall clock set back says nothing).
    fn idle(&self) -> Duration {
        self.at.elapsed().max(self.wall.elapsed().unwrap_or_default())
    }
}

/// What [`on_foreground`] needs of a connection: a seam, so it can be tested without a server.
trait Foreground {
    type Session: Send;
    fn connect(&mut self) -> impl std::future::Future<Output = Result<Self::Session>> + Send;
    /// Whether the connection still answers, found out in seconds ([`imap::probe`]).
    fn probe(&mut self, session: &mut Self::Session) -> impl std::future::Future<Output = Result<()>> + Send;
    fn run(&mut self, session: &mut Self::Session, job: &Job) -> impl std::future::Future<Output = Result<()>> + Send;
}

struct ImapForeground<'a> {
    shared: &'a Arc<Shared>,
    account: &'a Account,
}

impl Foreground for ImapForeground<'_> {
    type Session = Session;

    async fn connect(&mut self) -> Result<Session> {
        let cred = self.shared.credential(self.account).await?;
        imap::connect(self.account, &cred).await
    }

    async fn probe(&mut self, session: &mut Session) -> Result<()> {
        imap::probe(session).await
    }

    async fn run(&mut self, session: &mut Session, job: &Job) -> Result<()> {
        foreground_job(self.shared, self.account, session, job).await
    }
}

/// Runs `job` on the foreground connection `session`, opening one if there is none; leaves in
/// `session` what the next job can use. A connection left open since an earlier job may have
/// been dropped meanwhile (servers and NATs close idle connections early, GreenMail after
/// 30 s). Dropped without a word (the computer slept, a NAT forgot it), it never answers, and
/// the job would wait out the command's 30 s: one `idle` past [`PROBE_AFTER`] is probed first,
/// and replaced at once if it doesn't answer within seconds. Closed by the server, the job
/// fails on it for want of a connection, and runs once more on a fresh one, rather than wait
/// behind the background connection's work. An error means it didn't run here: the caller
/// hands it to the background connection.
async fn on_foreground<F: Foreground>(fg: &mut F, session: &mut Option<F::Session>, idle: Duration, job: &Job) -> Result<()> {
    if idle > PROBE_AFTER
        && let Some(s) = session.as_mut()
        && let Err(e) = fg.probe(s).await
    {
        log::info!("the foreground connection didn't answer after {}s idle ({e:#}): opening a new one", idle.as_secs());
        *session = None;
    }
    let fresh = session.is_none();
    let s = match session {
        Some(s) => s,
        None => session.insert(fg.connect().await?),
    };
    let Err(e) = fg.run(s, job).await else { return Ok(()) };
    *session = None;
    if fresh || !imap::is_lost(&e) {
        return Err(e);
    }
    log::info!("the foreground connection was gone ({e:#}): trying a new one");
    let mut s = fg.connect().await?;
    fg.run(&mut s, job).await?;
    *session = Some(s);
    Ok(())
}

async fn foreground_job(shared: &Arc<Shared>, account: &Account, session: &mut Session, job: &Job) -> Result<()> {
    let id = &account.id;
    match job {
        Job::Bodies { mailbox, validity, uids, reply: then } => {
            download_bodies(shared, account, session, mailbox, *validity, uids).await?;
            if let Some(then) = then {
                reply(shared, then)?;
            }
            shared.emit(Event::Changed { account: id.clone() });
        }
        Job::SyncMailbox(mailbox) => {
            sync_one(shared, account, session, mailbox).await?;
            // Bodies for the top of the list, so the first messages opened are instant.
            let max_size = shared.setting_u32("sync.maxPrefetchSizeKb").saturating_mul(1024);
            let (validity, missing) = shared.with_writer(|st| Ok((st.uidvalidity(id, mailbox)?, st.missing_bodies(id, mailbox, 25, max_size)?)))?;
            if !missing.is_empty() {
                download_bodies(shared, account, session, mailbox, validity, &missing).await?;
                shared.emit(Event::Changed { account: id.clone() });
            }
        }
        Job::Sync | Job::Folders => {}
    }
    Ok(())
}

/// Syncs one mailbox now (someone is looking at it), and its counts.
async fn sync_one(shared: &Arc<Shared>, account: &Account, session: &mut Session, mailbox: &str) -> Result<()> {
    let id = &account.id;
    let window = shared.setting_u32("sync.initialMessages").max(50);
    let progress = {
        let shared = shared.clone();
        let id = id.clone();
        move || shared.emit(Event::Changed { account: id.clone() })
    };
    let out = imap::sync_mailbox(session, &shared.writer, id, mailbox, window, &progress).await?;
    shared.synced(id, mailbox, out.contents);
    // Counts are a nicety; a lost connection isn't, and ends the job here.
    match imap::status(session, mailbox).await {
        Ok((unread, total)) => shared.with_writer(|st| st.set_counts(id, mailbox, unread, total))?,
        Err(e) if imap::is_lost(&e) => return Err(e),
        Err(e) => log::debug!("{}: no counts for {mailbox}: {e:#}", account.email),
    }
    shared.emit_mailboxes(id);
    if out.changed {
        shared.emit(Event::Changed { account: id.clone() });
    }
    Ok(())
}

/// How often the background connection looks at the watched mailboxes IDLE doesn't cover, a
/// STATUS each ([`changed_since_synced`]), while one is watched.
const LOOK_EVERY: Duration = Duration::from_secs(30);

/// Which of `mailboxes` changed since their last sync ([`Shared::synced`]), by a STATUS each
/// ([`imap::contents`]); one with no sync on record counts as changed. One the server won't
/// tell of (deleted meanwhile, say) doesn't; a lost connection ends it.
async fn changed_since_synced(shared: &Shared, account: &str, session: &mut imap::Connection<impl imap::Io>, mailboxes: &[String]) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for m in mailboxes {
        match imap::contents(session, m).await {
            Ok(now) => {
                let synced = shared.synced.lock().ok().and_then(|s| s.get(&(account.to_owned(), m.clone())).copied());
                if synced.is_none_or(|was| was.moved_on(now)) {
                    log::debug!("{account}: {m} changed since it was synced");
                    changed.push(m.clone());
                }
            }
            Err(e) if imap::is_lost(&e) => return Err(e),
            Err(e) => log::debug!("{account}: couldn't look at {m}: {e:#}"),
        }
    }
    Ok(changed)
}

/// Downloads bodies whose UIDs are from generation `validity` of `mailbox`
/// ([`imap::fetch_bodies`]). When the mailbox is in another generation now, a UID may name
/// another message: nothing is stored, and the mailbox is synced again instead, so its cache
/// starts over in the new one (the messages come back as new rows, to download when opened).
/// Returns whether the bodies were downloaded.
async fn download_bodies(shared: &Arc<Shared>, account: &Account, session: &mut Session, mailbox: &str, validity: u32, uids: &[u32]) -> Result<bool> {
    match imap::fetch_bodies(session, &shared.writer, &account.id, mailbox, validity, uids).await {
        Ok(_) => Ok(true),
        Err(e) if e.downcast_ref::<imap::NewGeneration>().is_some() => {
            log::info!("{}: {e:#}: syncing it again instead of downloading", account.email);
            sync_one(shared, account, session, mailbox).await?;
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// One account's sync loop: connect, catch up, replay queued changes, download ahead, then
/// wait (IMAP IDLE when available) until something changes. Reconnects with backoff.
async fn worker(shared: Arc<Shared>, id: String, mut jobs: mpsc::UnboundedReceiver<Job>, notify: Arc<Notify>) {
    let mut backoff = 2u64;
    let mut pending_jobs: Vec<Job> = Vec::new();
    let mut snoozes = SnoozeClock::default();
    loop {
        let Some(account) = shared.account(&id) else { return };
        shared.emit(Event::Sync { account: id.clone(), state: SyncState::Connecting });
        let session = match shared.credential(&account).await {
            Ok(cred) => imap::connect(&account, &cred).await,
            Err(e) => Err(e),
        };
        let session = match session {
            Ok(s) => {
                backoff = 2;
                s
            }
            Err(e) => {
                log::warn!("{}: connect failed: {e:#}", account.email);
                let auth = imap::is_auth_error(&e) || e.to_string().contains("refused") || e.to_string().contains("No password");
                let msg = format!("{e:#}");
                shared.emit(Event::Sync { account: id.clone(), state: if auth { SyncState::Error(msg) } else { SyncState::Offline(msg) } });
                // Sign-in problems won't fix themselves quickly; network ones might.
                let wait = if auth { 300 } else { backoff };
                backoff = (backoff * 2).min(120);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                    _ = notify.notified() => {}
                    j = jobs.recv() => match j { Some(j) => pending_jobs.push(j), None => return },
                }
                continue;
            }
        };
        match session_loop(&shared, &account, session, &mut jobs, &mut pending_jobs, &notify, &mut snoozes).await {
            Ok(()) => return,
            Err(e) => {
                log::warn!("{}: {e:#}", account.email);
                shared.emit(Event::Sync { account: id.clone(), state: SyncState::Offline(format!("{e:#}")) });
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(120);
            }
        }
    }
}

async fn session_loop(
    shared: &Arc<Shared>,
    account: &Account,
    mut session: Session,
    jobs: &mut mpsc::UnboundedReceiver<Job>,
    pending: &mut Vec<Job>,
    notify: &Arc<Notify>,
    snoozes: &mut SnoozeClock,
) -> Result<()> {
    let id = account.id.clone();
    let caps = imap::capabilities(&mut session).await?;
    let mut last_full: Option<Instant> = None;
    let mut last_list: Option<Instant> = None;
    let mut inbox_dirty = true;
    // The inbox as its last sync found it: mail that came after is checked for before IDLE.
    let mut inbox_synced: Option<imap::Contents> = None;
    // The watched mailboxes IDLE doesn't cover, and when they were last synced or looked at.
    let (mut looked, mut last_look) = (Vec::<String>::new(), Instant::now());
    loop {
        while let Ok(j) = jobs.try_recv() {
            pending.push(j);
        }
        // Opened messages first: someone is waiting.
        let (bodies, rest): (Vec<Job>, Vec<Job>) = pending.drain(..).partition(|j| matches!(j, Job::Bodies { .. }));
        for job in bodies {
            if let Job::Bodies { mailbox, validity, uids, reply: then } = job {
                download_bodies(shared, account, &mut session, &mailbox, validity, &uids).await?;
                if let Some(then) = &then {
                    reply(shared, then)?;
                }
                shared.emit(Event::Changed { account: id.clone() });
            }
        }
        let mut sync_boxes: Vec<String> = Vec::new();
        for job in rest {
            match job {
                Job::SyncMailbox(m) => sync_boxes.push(m),
                Job::Sync => inbox_dirty = true,
                Job::Folders => {
                    inbox_dirty = true;
                    last_list = None;
                }
                Job::Bodies { .. } => {}
            }
        }

        // Replay queued changes.
        log::debug!("{}: replaying ops", account.email);
        let ran_ops = replay_ops(shared, account, &mut session, caps).await?;

        let interval = Duration::from_secs(shared.setting_u32("sync.intervalSeconds").max(15) as u64);
        let full_due = last_full.is_none_or(|t| t.elapsed() >= interval);
        let window = shared.setting_u32("sync.initialMessages").max(50);
        let mut changed = ran_ops;

        if last_list.is_none_or(|t| t.elapsed() >= Duration::from_secs(600)) {
            shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing("Folders".into()) });
            let listed = imap::list_mailboxes(&mut session).await?;
            shared.with_writer(|st| store_listing(st, &id, &listed))?;
            last_list = Some(Instant::now());
            shared.emit_mailboxes(&id);
        }

        // Snoozes whose time has come go back to the inbox, by every Envoy that's running; old
        // empty buckets go. The buckets woken are synced, so their cached copies empty too.
        let snoozed = snooze_cycle(shared, account, &mut ImapSnooze { session: &mut session, caps }, snoozes, caps.moves, now()).await?;
        if !snoozed.woke.is_empty() {
            inbox_dirty = true;
            changed = true;
            sync_boxes.extend(snoozed.woke);
        }
        if !snoozed.deleted.is_empty() {
            shared.emit_mailboxes(&id);
        }

        let mailboxes = shared.with_writer(|st| st.mailboxes(&id))?;
        let watched: Vec<String> =
            shared.watched.lock().map(|w| w.iter().filter(|(a, _)| *a == id).map(|(_, m)| m.clone()).collect()).unwrap_or_default();
        // Which mailboxes to sync this round.
        let mut targets: Vec<String> = Vec::new();
        let mut push = |name: &str| {
            if !targets.iter().any(|t| t == name) {
                targets.push(name.to_owned());
            }
        };
        for m in &sync_boxes {
            push(m);
        }
        if inbox_dirty || full_due {
            if let Some(inbox) = mailboxes.iter().find(|m| m.role == Role::Inbox) {
                push(&inbox.name);
            }
            for w in &watched {
                push(w);
            }
        }
        if full_due {
            for m in mailboxes.iter().filter(|m| m.selectable && matches!(m.role, Role::Sent | Role::Flagged | Role::Drafts)) {
                push(&m.name);
            }
        }
        // Watched mailboxes IDLE doesn't cover (any but the inbox), between their syncs with the
        // inbox: looked at every LOOK_EVERY, and as soon as ops replayed (one may have moved mail
        // in), and synced if they changed. Mail filed into the one on screen, by another client
        // or from another folder, shows in seconds rather than at the next full sync. One just
        // opened is being synced by the foreground connection: its first look comes later.
        let mut beyond_idle: Vec<String> = watched.iter().filter(|w| mailboxes.iter().all(|m| m.role != Role::Inbox || m.name != w.as_str())).cloned().collect();
        beyond_idle.sort();
        if inbox_dirty || full_due || beyond_idle != looked {
            looked.clone_from(&beyond_idle);
            last_look = Instant::now();
        } else if !beyond_idle.is_empty() && (ran_ops || last_look.elapsed() >= LOOK_EVERY) {
            for m in changed_since_synced(shared, &id, &mut session, &beyond_idle).await? {
                push(&m);
            }
            last_look = Instant::now();
        }
        for name in &targets {
            shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing(display_of(&mailboxes, name)) });
            let progress = {
                let shared = shared.clone();
                let id = id.clone();
                move || shared.emit(Event::Changed { account: id.clone() })
            };
            let out = imap::sync_mailbox(&mut session, &shared.writer, &id, name, window, &progress).await?;
            shared.synced(&id, name, out.contents);
            if mailboxes.iter().any(|m| m.role == Role::Inbox && m.name == *name) {
                inbox_synced = Some(out.contents);
            }
            changed |= out.changed;
            if out.changed {
                shared.emit(Event::Changed { account: id.clone() });
            }
        }
        inbox_dirty = false;
        if full_due {
            for m in mailboxes.iter().filter(|m| m.selectable) {
                match imap::status(&mut session, &m.name).await {
                    Ok((unread, total)) => shared.with_writer(|st| st.set_counts(&id, &m.name, unread, total))?,
                    Err(e) if imap::is_lost(&e) => return Err(e),
                    Err(e) => log::debug!("{}: no counts for {}: {e:#}", account.email, m.name),
                }
            }
            last_full = Some(Instant::now());
            shared.emit_mailboxes(&id);
        } else if changed {
            shared.emit_mailboxes(&id);
        }

        // Download ahead, a batch at a time, so opened messages never wait long behind it.
        let prefetch = shared.setting_u32("sync.prefetchBodies");
        let max_size = shared.setting_u32("sync.maxPrefetchSizeKb").saturating_mul(1024);
        let mut more_to_fetch = false;
        if prefetch > 0 {
            let synced: Vec<String> = shared.with_writer(|st| {
                Ok(st.mailboxes(&id)?.into_iter().filter(|m| m.selectable && (targets.contains(&m.name) || m.role == Role::Inbox || watched.contains(&m.name))).map(|m| m.name).collect())
            })?;
            for name in synced {
                let (validity, missing) = shared.with_writer(|st| Ok((st.uidvalidity(&id, &name)?, st.missing_bodies(&id, &name, prefetch, max_size)?)))?;
                if missing.is_empty() {
                    continue;
                }
                shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing(format!("Downloading {}", missing.len())) });
                let batch: Vec<u32> = missing.iter().take(20).copied().collect();
                // Reset since: synced again instead, and the next round downloads ahead in it.
                let downloaded = download_bodies(shared, account, &mut session, &name, validity, &batch).await?;
                shared.emit(Event::Changed { account: id.clone() });
                more_to_fetch = downloaded && missing.len() > batch.len();
                break;
            }
        }
        if more_to_fetch || !jobs.is_empty() {
            continue;
        }

        shared.emit(Event::Sync { account: id.clone(), state: SyncState::Idle });
        shared.emit_pending();
        // Wait for news: IDLE on the inbox, a command, the next interval or a delayed send.
        let due = shared.with_writer(|st| st.next_op_due(&id, now()))?;
        let mut wait = last_full.map_or(Duration::ZERO, |t| interval.saturating_sub(t.elapsed()));
        if let Some(d) = due {
            // One already due goes now.
            wait = wait.min(Duration::from_secs(d.max(0) as u64));
        }
        // Retry failed ops soon.
        let failed_due = shared.with_writer(|st| st.ops(&id, now() + 3600).map(|o| !o.is_empty()))?;
        if failed_due {
            wait = wait.min(Duration::from_secs(30));
        }
        // The next look at the watched mailboxes IDLE doesn't cover.
        if !beyond_idle.is_empty() {
            wait = wait.min(LOOK_EVERY.saturating_sub(last_look.elapsed()));
        }
        // The next snooze to wake.
        if let Some(at) = snoozes.next(&shared.with_writer(|st| st.buckets(&id))?, caps.moves) {
            wait = wait.min(Duration::from_secs((at - now()).max(0) as u64));
        }
        if wait.is_zero() {
            continue;
        }
        let inbox = mailboxes.iter().find(|m| m.role == Role::Inbox).map(|m| m.name.clone());
        log::debug!("{}: waiting up to {wait:?}", account.email);
        if caps.idle && shared.setting_bool("sync.useIdle") && inbox.is_some() {
            // IDLE reports only what comes once it's started: mail delivered since the inbox's
            // last sync (while ops replayed, say: a message sent to oneself) is synced first, or
            // it would wait for the next interval.
            if imap::changed_since(&mut session, inbox.as_deref().unwrap_or("INBOX"), inbox_synced).await? {
                log::debug!("{}: the inbox changed since it was synced", account.email);
                inbox_dirty = true;
                continue;
            }
            // Servers drop IDLE after ~30 minutes; renew well before.
            let wait = wait.min(Duration::from_secs(25 * 60));
            let (s, news) = imap::idle(session, wait, notify.clone()).await?;
            log::debug!("{}: idle ended (news: {news})", account.email);
            session = s;
            if news {
                inbox_dirty = true;
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = notify.notified() => {}
            }
        }
    }
}

/// Records the account's mailboxes as a LIST named them, forgetting (with their cached mail)
/// the ones it no longer names: only when it names INBOX, which every server has. A listing
/// without it isn't the whole list (whatever cut it short), and nothing is forgotten on it.
fn store_listing(st: &Store, account: &str, listed: &[imap::ListedMailbox]) -> Result<()> {
    for m in listed {
        st.upsert_mailbox(account, &m.name, &m.display, m.role, &m.delimiter, m.selectable)?;
    }
    if !listed.iter().any(|m| m.role == Role::Inbox) {
        log::warn!("{account}: the server's mailbox list didn't name INBOX; no mailbox is forgotten on it");
        return Ok(());
    }
    // Where snoozes go, decided from the whole list, as every Envoy decides it.
    let parent = imap::snooze_parent(listed);
    st.set_snooze_parent(account, &parent.name, &parent.delimiter)?;
    st.retain_mailboxes(account, &listed.iter().map(|m| m.name.clone()).collect::<Vec<_>>())
}

fn display_of(mailboxes: &[Mailbox], name: &str) -> String {
    mailboxes.iter().find(|m| m.name == name).map_or_else(|| name.to_owned(), |m| m.display.clone())
}

async fn replay_ops(shared: &Arc<Shared>, account: &Account, session: &mut Session, caps: imap::Caps) -> Result<bool> {
    replay_with(shared, account, &mut ImapRunner { shared, account, session, caps, buckets: HashSet::new() }).await
}

/// Carries out queued ops on the server. A seam so replay can be tested without a server.
trait OpRunner {
    /// Every op but the draft ones, which replay through the two calls below. `at`: which
    /// generation of its mailbox the op's UID is from; when the mailbox is in another now, the
    /// op doesn't run and the error is [`imap::NewGeneration`].
    fn run(&mut self, op: &Op, at: &At) -> impl std::future::Future<Output = Result<()>> + Send;
    /// Hands an [`Op::Send`]'s message to the SMTP server.
    fn deliver(&mut self, op: &Op) -> impl std::future::Future<Output = Result<()>> + Send;
    /// Files a copy of a sent message in `mailbox` (APPEND).
    fn file_sent(&mut self, mailbox: &str, raw: &[u8]) -> impl std::future::Future<Output = Result<()>> + Send;
    /// The server's (unread, total) for a mailbox.
    fn status(&mut self, mailbox: &str) -> impl std::future::Future<Output = Result<(u32, u32)>> + Send;
    /// Puts a draft in `mailbox`, replacing its `previous` copy ([`imap::replace_draft`]).
    fn replace_draft(&mut self, mailbox: &str, raw: &[u8], message_id: &str, previous: Option<(u32, u32)>, maybe_strays: bool) -> impl std::future::Future<Output = Result<imap::Replaced>> + Send;
    /// Deletes a draft's copies from `mailbox`: `known`, and, with `search`, any with its
    /// Message-ID. Returns their UIDs.
    fn purge_draft(&mut self, mailbox: &str, message_id: &str, known: Option<(u32, u32)>, search: bool) -> impl std::future::Future<Output = Result<Vec<u32>>> + Send;
    /// Makes sure snooze bucket `bucket`, under `parent`, is on the server
    /// ([`imap::ensure_bucket`]): [`imap::CantCreate`] if the server won't have it.
    fn create_bucket(&mut self, parent: &str, bucket: &str) -> impl std::future::Future<Output = Result<()>> + Send;
}

struct ImapRunner<'a> {
    shared: &'a Arc<Shared>,
    account: &'a Account,
    session: &'a mut Session,
    caps: imap::Caps,
    /// Buckets made (or found) this pass: a pass of snoozes to one time makes it once.
    buckets: HashSet<String>,
}

impl OpRunner for ImapRunner<'_> {
    async fn create_bucket(&mut self, parent: &str, bucket: &str) -> Result<()> {
        if !self.buckets.contains(bucket) {
            imap::ensure_bucket(self.session, parent, bucket).await?;
            self.buckets.insert(bucket.to_owned());
        }
        Ok(())
    }

    async fn status(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        imap::status(self.session, mailbox).await
    }

    async fn replace_draft(&mut self, mailbox: &str, raw: &[u8], message_id: &str, previous: Option<(u32, u32)>, maybe_strays: bool) -> Result<imap::Replaced> {
        imap::replace_draft(self.session, self.caps, mailbox, raw, message_id, previous, maybe_strays).await
    }

    async fn purge_draft(&mut self, mailbox: &str, message_id: &str, known: Option<(u32, u32)>, search: bool) -> Result<Vec<u32>> {
        imap::purge_message_id(self.session, self.caps, mailbox, message_id, known, search).await
    }

    async fn run(&mut self, op: &Op, at: &At) -> Result<()> {
        let session = &mut *self.session;
        let target = |uid: u32| imap::Target { uid, validity: at.validity, message_id: &at.message_id };
        match op {
            Op::SetFlags { mailbox, uid, add, remove } => imap::store_flags(session, mailbox, &target(*uid), *add, *remove).await,
            Op::Move { mailbox, uid, dest } => imap::move_message(session, self.caps, mailbox, &target(*uid), dest).await,
            Op::Copy { mailbox, uid, dest } => imap::copy_message(session, mailbox, &target(*uid), dest).await,
            Op::Expunge { mailbox, uid } => imap::expunge(session, self.caps, mailbox, &target(*uid)).await,
            Op::SaveDraft { .. } | Op::PurgeDraft { .. } => Err(anyhow!("draft ops replay through replay_draft")),
            Op::Send { .. } | Op::Delivered { .. } => Err(anyhow!("sends replay through deliver_and_file")),
        }
    }

    async fn deliver(&mut self, op: &Op) -> Result<()> {
        let (shared, account) = (self.shared, self.account);
        let Op::Send { raw_b64, from, to, .. } = op else { return Err(anyhow!("not a send")) };
        let raw = base64::engine::general_purpose::STANDARD.decode(raw_b64)?;
        let cred = shared.credential(account).await?;
        log::info!("{}: sending to {} recipient(s)", account.email, to.len());
        let mut size = None;
        let sent = smtp::send(account, &cred, from, to, &raw, &mut size).await;
        // Kept whether it went or not: the next message is judged by it. (It can't fail the
        // send: a message the server took is reported sent.)
        shared.learned_size(&account.id, size);
        sent?;
        log::info!("{}: sent", account.email);
        Ok(())
    }

    async fn file_sent(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        imap::append(self.session, mailbox, raw, true).await
    }
}

/// Replays an [`Op::Send`]: hands its message to the SMTP server, and once that's done
/// records it at once ([`Op::Delivered`]), before anything else can fail, retrying until the
/// record sticks, so the message is never sent again, whatever happens to filing it. In the same transaction the message it
/// replies to is queued to be marked answered, on its own account, by its UID only in the
/// generation it was pinned down in (or where its Message-ID finds it in a new one); a reply
/// whose original couldn't be pinned down marks nothing.
async fn deliver_and_file(shared: &Shared, account: &Account, runner: &mut impl OpRunner, q: &mut QueuedOp) -> Result<()> {
    runner.deliver(&q.op).await?;
    let Op::Send { raw_b64, sent_mailbox, reply_of, .. } = &q.op else { return Ok(()) };
    let delivered = Op::Delivered { raw_b64: raw_b64.clone(), sent_mailbox: sent_mailbox.clone() };
    let answered = match reply_of {
        Some(ReplyOf::Original(o)) if shared.account(&o.account).is_some() => Some(o),
        Some(ReplyOf::Original(o)) => {
            log::info!("{}: the message replied to was on {}, which is gone: not marked answered", account.email, o.account);
            None
        }
        Some(ReplyOf::Legacy(..)) => {
            log::info!("{}: a reply queued by an older Envoy doesn't say which message it answers for sure: not marked answered", account.email);
            None
        }
        None => None,
    };
    // The message is out: from here this op never goes back to SMTP. Recording that is
    // retried, with backoff, until it sticks (a failure here isn't a failure to send, and
    // replay's retry would send it again); only a crash before then could.
    let mut wait = Duration::from_millis(100);
    loop {
        let recorded = shared.with_writer(|st| {
            st.transaction(|st| {
                st.rewrite_op(q.id, &delivered)?;
                if let Some(o) = answered {
                    let flag = Op::SetFlags { mailbox: o.mailbox.clone(), uid: o.uid, add: flags::ANSWERED, remove: 0 };
                    st.enqueue_located(&o.account, &flag, &At { validity: o.validity, message_id: o.message_id.clone() })?;
                }
                Ok(())
            })
        });
        match recorded {
            Ok(()) => break,
            Err(e) => {
                log::error!("{}: a message went out, but recording that failed ({e:#}); trying again in {wait:?}", account.email);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    }
    q.op = delivered;
    shared.emit(Event::Toast("Message sent".into()));
    file_delivered(shared, account, runner, q).await
}

/// What's left of a send once its message is out: a copy in Sent. Whatever filing it meets,
/// the message is never sent again. A lost connection or a "not now" keeps the op queued, to
/// file on a later try, with backoff (replay never gives up on it). A refusal for good, or
/// one without a reason that keeps coming, keeps the message here instead
/// ([`Store::keep_unfiled`]), and says so once.
async fn file_delivered(shared: &Shared, account: &Account, runner: &mut impl OpRunner, q: &QueuedOp) -> Result<()> {
    let Op::Delivered { raw_b64, sent_mailbox: Some(sent) } = &q.op else { return Ok(()) };
    let raw = match base64::engine::general_purpose::STANDARD.decode(raw_b64) {
        Ok(raw) => raw,
        Err(e) => {
            log::warn!("{}: couldn't save to Sent: {e}", account.email);
            return Ok(());
        }
    };
    let Err(e) = runner.file_sent(sent, &raw).await else { return Ok(()) };
    let for_good = !imap::is_lost(&e)
        && match imap::failure(&e) {
            imap::Failure::Temporary => false,
            imap::Failure::Permanent => true,
            imap::Failure::Unknown => q.attempts >= 5,
        };
    if !for_good {
        return Err(e);
    }
    let why = format!("{e:#}");
    shared.with_writer(|st| st.keep_unfiled(q.id, &account.id, sent, &raw, &why, now()))?;
    log::warn!("{}: {sent} refused a sent message's copy for good ({why}): kept here", account.email);
    shared.emit(Event::Error(format!("Your message was sent, but {} wouldn't save a copy in {sent} ({why}). The copy is kept on this computer.", account.email)));
    Ok(())
}

/// Replays the account's due ops, in order, retrying failures with backoff. True when any
/// op finished (or was given up on).
async fn replay_with(shared: &Arc<Shared>, account: &Account, runner: &mut impl OpRunner) -> Result<bool> {
    let r = replay_claimed(shared, account, runner).await;
    // Whatever didn't get its turn (connection lost, an error) goes back in the queue, where
    // undo can reach it again.
    shared.with_writer(|st| st.unclaim(&account.id))?;
    r
}

async fn replay_claimed(shared: &Arc<Shared>, account: &Account, runner: &mut impl OpRunner) -> Result<bool> {
    // Claimed: an undo from here on can't cancel these, since the server may be doing them.
    let ops = shared.with_writer(|st| st.claim_due(&account.id, now()))?;
    let mut any = false;
    // Mailboxes moved out of or copied into, or whose unread messages changed: their counts get
    // re-read once the pass is done.
    let mut recount: Vec<String> = Vec::new();
    for mut q in ops {
        let mut r = match &q.op {
            Op::SaveDraft { .. } | Op::PurgeDraft { .. } => replay_draft(shared, account, runner, &q.op).await,
            Op::Send { .. } => deliver_and_file(shared, account, runner, &mut q).await,
            Op::Delivered { .. } => file_delivered(shared, account, runner, &q).await,
            _ => run_located(shared, account, runner, &mut q).await,
        };
        // The op's mailbox has a new generation since it was queued: its UID may name another
        // message now, and it didn't run. Its message found again by Message-ID, it runs there;
        // not found (or found twice), it's dropped, and what it hid shows again.
        if let Err(e) = &r
            && let Some(g) = e.downcast_ref::<imap::NewGeneration>()
        {
            // An expunge from an unknown generation never runs: deleting needs proof.
            let found = g.found.filter(|_| !(q.at.validity == 0 && matches!(q.op, Op::Expunge { .. })));
            match found.and_then(|uid| q.op.at_uid(uid)) {
                Some(op) => {
                    log::info!("{}: {} was reset since a change was queued; its message was found again by Message-ID", account.email, g.mailbox);
                    let validity = g.validity;
                    shared.with_writer(|st| st.relocate_op(q.id, &op, validity))?;
                    q.op = op;
                    q.at.validity = validity;
                    r = run_located(shared, account, runner, &mut q).await;
                }
                None => {
                    let mailbox = g.mailbox.clone();
                    log::warn!("{}: dropped a change: {e:#}, and its message wasn't found again", account.email);
                    shared.with_writer(|st| st.op_dropped(q.id))?;
                    any = true;
                    shared.emit(Event::Changed { account: account.id.clone() });
                    shared.emit(Event::Error(format!(
                        "A change to a message in {mailbox} on {} was dropped: the folder was reset on the server, and the message couldn't be found in it again.",
                        account.email
                    )));
                    continue;
                }
            }
        }
        match r {
            Ok(()) => {
                shared.with_writer(|st| st.op_done(q.id))?;
                any = true;
                let changed = match &q.op {
                    Op::Move { mailbox, .. } | Op::Expunge { mailbox, .. } => Some(mailbox),
                    Op::Copy { dest, .. } => Some(dest),
                    Op::SetFlags { mailbox, add, remove, .. } if (add | remove) & flags::SEEN != 0 => Some(mailbox),
                    _ => None,
                };
                if let Some(mailbox) = changed
                    && !recount.contains(mailbox)
                {
                    recount.push(mailbox.clone());
                }
            }
            // The connection went (by the error's type, not its words): the op goes back in the
            // queue as it was, no attempt counted, and the pass stops so the worker connects
            // again. However often the network fails, a valid change is never given up on for it.
            Err(e) if imap::is_lost(&e) => return Err(e),
            Err(e) => {
                let msg = format!("{e:#}");
                let draft_op = matches!(q.op, Op::SaveDraft { .. } | Op::PurgeDraft { .. });
                // A message too big for its server: no retry changes that. Off the queue, back
                // in the composer, saying why.
                if let (Op::Send { .. }, Some(too_big)) = (&q.op, smtp::too_big(&e)) {
                    let why = too_big.to_string();
                    shared.with_writer(|st| {
                        if let Some(d) = st.send_bounced(q.id, now())? {
                            shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: false, notice: Some(DraftNotice::NotSent(why.clone())) });
                        }
                        Ok(())
                    })?;
                    shared.emit(Event::Error(why));
                    shared.emit(Event::Changed { account: account.id.clone() });
                    any = true;
                    continue;
                }
                // A message the server rejects for good (gone, no such mailbox) is dropped
                // after a few tries; network trouble keeps it queued. Sends and drafts are
                // never given up on: giving up would leave a draft off the server (its only
                // upload gone), or bring back a draft that was sent.
                if draft_op {
                    let retry = 15i64 << q.attempts.min(6);
                    shared.with_writer(|st| st.op_failed(q.id, &msg, retry, now()))?;
                    // Quiet about trouble that clears by itself; told about refusals, again
                    // every few tries.
                    let tell = match imap::failure(&e) {
                        imap::Failure::Temporary => false,
                        imap::Failure::Permanent => q.attempts % 5 == 0,
                        imap::Failure::Unknown => q.attempts % 5 == 4,
                    };
                    if tell {
                        shared.emit(Event::Error(match q.op {
                            Op::SaveDraft { .. } => format!("Couldn't save a draft to Drafts on {} ({msg}). It's safe on this computer, and Envoy keeps trying.", account.email),
                            _ => format!("Couldn't remove a draft from Drafts on {} ({msg}). Envoy keeps trying.", account.email),
                        }));
                    }
                } else if q.attempts >= 5 && !matches!(q.op, Op::Send { .. } | Op::Delivered { .. }) {
                    // A move that never happened: its message shows again where it still is.
                    shared.with_writer(|st| st.op_dropped(q.id))?;
                    any = true;
                    shared.emit(Event::Changed { account: account.id.clone() });
                    shared.emit(Event::Error(format!("Gave up on a change for {}: {msg}", account.email)));
                } else {
                    let retry = 15i64 << q.attempts.min(6);
                    shared.with_writer(|st| st.op_failed(q.id, &msg, retry, now()))?;
                    if matches!(q.op, Op::Send { .. }) {
                        shared.emit(Event::Error(format!("Couldn't send yet ({msg}). Envoy will keep trying.")));
                    }
                }
            }
        }
    }
    // A STATUS from the foreground connection can land between the server carrying out a
    // move and op_done recording it, and then counts the message out twice (it's gone on the
    // server, and still held here). Re-reading the counts now, with nothing in flight, puts
    // the badge right whichever way that raced.
    for mailbox in &recount {
        match runner.status(mailbox).await {
            Ok((unread, total)) => shared.with_writer(|st| st.set_counts(&account.id, mailbox, unread, total))?,
            Err(e) if imap::is_lost(&e) => return Err(e),
            Err(e) => log::debug!("{}: couldn't recount {mailbox}: {e:#}", account.email),
        }
    }
    if !recount.is_empty() {
        shared.emit_mailboxes(&account.id);
    }
    if any {
        shared.emit_pending();
    }
    Ok(any)
}

/// Runs an op that acts on a message by UID: a snooze through [`snooze_move`], the rest as
/// they are.
async fn run_located(shared: &Shared, account: &Account, runner: &mut impl OpRunner, q: &mut QueuedOp) -> Result<()> {
    if matches!(&q.op, Op::Move { dest, .. } if snooze::wake_of(dest).is_some()) {
        return snooze_move(shared, account, runner, q).await;
    }
    runner.run(&q.op, &q.at).await
}

/// Replays a snooze: a move into a bucket ([`crate::snooze`]). Its time too close, or past, it
/// never goes into the bucket: the message wakes instead, unread and pinned, where it is in the
/// inbox, or back in it from elsewhere. Otherwise the bucket is made (once a pass), under
/// `Envoy Snoozed` if the server won't have `Snoozed` (the op, and the account's choice, follow),
/// and known here at once, for its wake and the Snoozed view; then the message moves in.
async fn snooze_move(shared: &Shared, account: &Account, runner: &mut impl OpRunner, q: &mut QueuedOp) -> Result<()> {
    let Op::Move { mailbox, uid, dest } = q.op.clone() else { return runner.run(&q.op, &q.at).await };
    let (Some(wake), Some(parent)) = (snooze::wake_of(&dest), snooze::parent_of(&dest).map(str::to_owned)) else { return runner.run(&q.op, &q.at).await };
    if snooze::wakes_instead(wake, now()) {
        log::info!("{}: a snooze reached the server at its time ({dest}): woken instead", account.email);
        runner.run(&Op::SetFlags { mailbox: mailbox.clone(), uid, add: flags::WOKEN, remove: flags::SEEN }, &q.at).await?;
        let inbox = shared.with_writer(|st| st.mailbox_for_role(&account.id, Role::Inbox))?;
        match inbox {
            Some(inbox) if inbox != mailbox => {
                runner.run(&Op::Move { mailbox, uid, dest: inbox }, &q.at).await?;
                // Replay's op_done takes the row it hid here, as for any move; the copy that
                // lands in the inbox comes pinned.
                shared.with_writer(|st| st.add_woken(&account.id, std::slice::from_ref(&q.at.message_id), now()))?;
            }
            // It stays where it is: shown again, unread and pinned. (Replay's op_done then
            // finds nothing left to do.)
            _ => shared.with_writer(|st| st.woke_in_place(q.id, now()))?,
        }
        return Ok(());
    }
    let delimiter = dest[parent.len()..].chars().next().map(String::from).unwrap_or_else(|| "/".to_owned());
    match runner.create_bucket(&parent, &dest).await {
        Ok(()) => {}
        Err(e) if e.downcast_ref::<imap::CantCreate>().is_some() && snooze::fallback_of(&parent).is_some() => {
            let fallback = snooze::fallback_of(&parent).unwrap_or_default();
            log::warn!("{}: {e:#}; snoozes go to {fallback} instead", account.email);
            let moved = snooze::bucket(&fallback, &delimiter, wake);
            runner.create_bucket(&fallback, &moved).await?;
            let op = Op::Move { mailbox, uid, dest: moved };
            shared.with_writer(|st| {
                st.transaction(|st| {
                    st.set_snooze_parent(&account.id, &fallback, &delimiter)?;
                    st.rewrite_op(q.id, &op)
                })
            })?;
            q.op = op;
        }
        Err(e) => return Err(e),
    }
    if let Op::Move { dest, .. } = &q.op {
        shared.with_writer(|st| st.add_bucket(&account.id, dest))?;
    }
    runner.run(&q.op, &q.at).await
}

/// What the snooze cycle needs of the server: a seam, so it can be tested without one.
trait SnoozeServer {
    /// Wakes a bucket into `inbox` ([`imap::wake_bucket`]).
    fn wake(&mut self, bucket: &str, inbox: &str) -> impl std::future::Future<Output = Result<imap::Woken>> + Send;
    /// Deletes a bucket the server says is empty ([`imap::cleanup_bucket`]): true when it's gone.
    fn cleanup(&mut self, bucket: &str, inbox: &str) -> impl std::future::Future<Output = Result<bool>> + Send;
}

struct ImapSnooze<'a> {
    session: &'a mut Session,
    caps: imap::Caps,
}

impl SnoozeServer for ImapSnooze<'_> {
    async fn wake(&mut self, bucket: &str, inbox: &str) -> Result<imap::Woken> {
        imap::wake_bucket(self.session, self.caps, bucket, inbox).await
    }

    async fn cleanup(&mut self, bucket: &str, inbox: &str) -> Result<bool> {
        imap::cleanup_bucket(self.session, bucket, inbox).await
    }
}

/// How often old empty buckets are looked for, and a woken bucket looked into again (a message
/// a far-off Envoy, its clock behind, put there late).
const SNOOZE_RECHECK: i64 = 600;
/// A wake the server refused is tried again after this long.
const WAKE_RETRY: i64 = 60;
/// Woken messages are pinned as they land for this long at most.
const KEEP_WOKEN: i64 = 30 * 24 * 3600;

/// A worker's snooze timing, kept across reconnects: this Envoy's jitter for each bucket (on a
/// server without MOVE), when a bucket may be woken again (one just woken, or one whose wake
/// failed), and when old buckets were last looked for.
#[derive(Default)]
struct SnoozeClock {
    jitter: HashMap<String, i64>,
    not_before: HashMap<String, i64>,
    last_cleanup: Option<i64>,
}

impl SnoozeClock {
    /// When `bucket`, waking at `wake`, is due here.
    fn due_at(&mut self, bucket: &str, wake: i64, moves: bool) -> i64 {
        let jitter = if moves { 0 } else { *self.jitter.entry(bucket.to_owned()).or_insert_with(snooze::jitter) };
        let at = wake + jitter;
        self.not_before.get(bucket).map_or(at, |t| at.max(*t))
    }

    /// The first time any of `buckets` (`(name, wake)`) is due here.
    fn next(&mut self, buckets: &[(String, i64)], moves: bool) -> Option<i64> {
        buckets.iter().map(|(b, w)| self.due_at(b, *w, moves)).min()
    }
}

/// What a [`snooze_cycle`] did: the buckets it woke (that had messages) and deleted.
#[derive(Debug, Default, PartialEq)]
struct SnoozeCycle {
    woke: Vec<String>,
    deleted: Vec<String>,
}

/// One round of the account's snoozes at `now`: every ten minutes, buckets a day past their
/// time that the server says are empty are deleted; then each bucket whose time has come here
/// ([`SnoozeClock::due_at`]) is woken into the inbox, and its messages remembered as woken.
/// A lost connection ends the round (the worker connects again and catches up); a refusal is
/// tried again in a minute.
async fn snooze_cycle(shared: &Shared, account: &Account, server: &mut impl SnoozeServer, clock: &mut SnoozeClock, moves: bool, now: i64) -> Result<SnoozeCycle> {
    let (inbox, buckets) = shared.with_writer(|st| Ok((st.mailbox_for_role(&account.id, Role::Inbox)?, st.buckets(&account.id)?)))?;
    let mut done = SnoozeCycle::default();
    let Some(inbox) = inbox else { return Ok(done) };
    if clock.last_cleanup.is_none_or(|t| now - t >= SNOOZE_RECHECK) {
        clock.last_cleanup = Some(now);
        for (bucket, _) in buckets.iter().filter(|(_, wake)| snooze::old_enough(*wake, now)) {
            match server.cleanup(bucket, &inbox).await {
                Ok(true) => {
                    log::info!("{}: deleted the empty snooze bucket {bucket}", account.email);
                    shared.with_writer(|st| st.forget_mailbox(&account.id, bucket))?;
                    clock.not_before.remove(bucket);
                    clock.jitter.remove(bucket);
                    done.deleted.push(bucket.clone());
                }
                Ok(false) => {}
                Err(e) if imap::is_lost(&e) => return Err(e),
                Err(e) => log::warn!("{}: couldn't clean up {bucket}: {e:#}", account.email),
            }
        }
        shared.with_writer(|st| st.prune_woken(now - KEEP_WOKEN))?;
    }
    for (bucket, wake) in buckets.iter().filter(|(b, _)| !done.deleted.contains(b)) {
        if now < clock.due_at(bucket, *wake, moves) {
            continue;
        }
        match server.wake(bucket, &inbox).await {
            Ok(woken) => {
                clock.not_before.insert(bucket.clone(), now + SNOOZE_RECHECK);
                if woken.moved > 0 {
                    log::info!("{}: woke {} snoozed message(s) from {bucket}", account.email, woken.moved);
                    shared.with_writer(|st| st.add_woken(&account.id, &woken.message_ids, now))?;
                    done.woke.push(bucket.clone());
                }
            }
            Err(e) if imap::is_lost(&e) => return Err(e),
            Err(e) => {
                log::warn!("{}: couldn't wake {bucket}: {e:#}; trying again in a minute", account.email);
                clock.not_before.insert(bucket.clone(), now + WAKE_RETRY);
            }
        }
    }
    Ok(done)
}

/// Where an account's drafts go. An error while its folders aren't known yet (the op is
/// retried); None when it has no Drafts mailbox, and drafts stay on this computer.
fn drafts_mailbox(shared: &Shared, account: &str) -> Result<Option<String>> {
    shared.with_writer(|st| {
        let boxes = st.mailboxes(account)?;
        anyhow::ensure!(!boxes.is_empty(), "the folder list isn't loaded yet");
        Ok(boxes.into_iter().find(|m| m.role == Role::Drafts && m.selectable).map(|m| m.name))
    })
}

/// Replays a draft op, reading and updating the local draft around the server work.
///
/// [`Op::SaveDraft`] uploads the draft's newest version, if the server's copy is behind and the
/// draft is staying (not being sent or discarded), replacing every older copy. [`Op::PurgeDraft`]
/// deletes the copies (none to look for if no upload was ever started), then the draft.
async fn replay_draft(shared: &Arc<Shared>, account: &Account, runner: &mut impl OpRunner, op: &Op) -> Result<()> {
    let id = &account.id;
    match op {
        Op::SaveDraft { draft } => {
            let Some(mailbox) = drafts_mailbox(shared, id)? else { return Ok(()) };
            let Some(up) = shared.with_writer(|st| st.draft_upload(id, *draft))? else { return Ok(()) };
            // A draft that can't be built (or lost a file) won't do better on a retry.
            let files = shared.with_writer(|st| st.attached_files(*draft)).map_err(|e| imap::Refused(format!("{e:#}")))?;
            let raw = smtp::build_draft(account, &up.draft, &up.message_id, files).map_err(|e| imap::Refused(format!("{e:#}")))?;
            // The copy on record is replaced by UID; one in another mailbox (the Drafts role
            // moved) is left to the search.
            let previous = up.previous.as_ref().filter(|(m, _, _)| *m == mailbox).map(|(_, validity, uid)| (*validity, *uid));
            let done = runner.replace_draft(&mailbox, &raw, &up.message_id, previous, up.maybe_strays).await?;
            shared.with_writer(|st| {
                // If the draft moved to another account meanwhile, this copy is queued for
                // deletion here instead of recorded.
                if !st.draft_uploaded(*draft, &up, &mailbox, done.copy, done.certain)? {
                    log::info!("{}: a draft moved to another account while it was uploaded here; its copy goes", account.email);
                }
                st.delete_uids(id, &mailbox, &done.deleted)
            })?;
        }
        Op::PurgeDraft { message_id, draft, copy, exact, left_by } => {
            let (on_server, known) = match draft {
                // The copy on record counts only if it's on this account's server.
                Some(d) => shared.with_writer(|st| {
                    let known = st.draft_copy(*d)?.filter(|(acct, ..)| acct == id).map(|(_, m, v, u)| (m, v, u));
                    Ok((st.draft_appended(*d)?, known.or_else(|| copy.clone())))
                })?,
                None => (true, copy.clone()),
            };
            if on_server && let Some(mailbox) = drafts_mailbox(shared, id)? {
                let known = known.filter(|(m, _, _)| *m == mailbox).map(|(_, validity, uid)| (validity, uid));
                let gone = runner.purge_draft(&mailbox, message_id, known, !*exact).await?;
                shared.with_writer(|st| {
                    st.delete_uids(id, &mailbox, &gone)?;
                    // A cleanup's search may have taken the copy of a newer upload here: on this
                    // try, or on one whose answer was lost (this one then finds nothing), so
                    // whatever it found.
                    if let Some((d, generation)) = left_by
                        && !*exact
                        && st.draft_cleaned_up(*d, id, *generation, now())?
                    {
                        log::info!("{}: an old cleanup may have taken a draft's newest copy; it goes up again", account.email);
                    }
                    Ok(())
                })?;
            }
            if let Some(d) = draft {
                shared.with_writer(|st| st.draft_purged(*d, now()))?;
            }
        }
        _ => return Err(anyhow!("not a draft op")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_level_logs_name_an_account_without_its_address() {
        let id = accounts::new_id("alice.smith@example.com");
        let tag = account_tag(&id);
        assert!(!tag.contains("alice") && !tag.contains("example"), "{tag}");
        assert_eq!((tag.len(), tag.clone()), ("account 0123abcd".len(), account_tag(&id)), "short, and the same each time");
        assert_ne!(tag, account_tag(&accounts::new_id("bob@example.com")));
        // Nor the mailbox a job is about.
        assert_eq!(Job::SyncMailbox("Clients/Acme".into()).kind(), "sync");
    }

    fn message(st: &Store, mailbox: &str, uid: u32, mid: &str, refs: &str, fl: u32) -> MessageId {
        message_in(st, "a", mailbox, uid, mid, refs, fl)
    }

    fn message_in(st: &Store, account: &str, mailbox: &str, uid: u32, mid: &str, refs: &str, fl: u32) -> MessageId {
        let raw = format!("From: Ana <ana@x.com>\r\nSubject: Plans\r\nMessage-ID: {mid}\r\nReferences: {refs}\r\nDate: Tue, 3 Mar 2026 0{uid}:00:00 +0000\r\n\r\nhi\r\n");
        let p = crate::mime::parse(raw.as_bytes(), false).unwrap();
        let id = st.upsert_header(account, mailbox, uid, fl, 10, 0, &p).unwrap();
        st.set_body(id, raw.as_bytes(), &p).unwrap();
        id
    }

    fn setup() -> Store {
        let st = Store::memory();
        for account in ["a", "b"] {
            for (name, role) in [("INBOX", Role::Inbox), ("Archive", Role::Archive), ("Trash", Role::Trash), ("Junk", Role::Junk), ("Drafts", Role::Drafts)] {
                st.upsert_mailbox(account, name, name, role, "/", true).unwrap();
            }
        }
        st
    }

    fn engine_with(st: Store) -> (Arc<Shared>, std::sync::mpsc::Receiver<Event>) {
        engine_with_reader(st, Store::memory())
    }

    /// With a reader of its own: the same database as `st`, for lists.
    fn engine_with_reader(st: Store, reader: Store) -> (Arc<Shared>, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            writer: Arc::new(Mutex::new(st)),
            reader: Arc::new(Mutex::new(reader)),
            events: Mutex::new(tx),
            waker: Arc::new(|| {}),
            settings: RwLock::new(Settings::default()),
            tokens: Mutex::new(HashMap::new()),
            accounts: RwLock::new(vec![]),
            watched: Mutex::new(HashSet::new()),
            synced: Mutex::new(HashMap::new()),
            undo_keys: Default::default(),
            wakers: Default::default(),
            category_undo: Default::default(),
            recategorizing: Default::default(),
        });
        (shared, rx)
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.into(),
            email: format!("{id}@x.com"),
            display_name: String::new(),
            auth: AuthKind::Password,
            username: String::new(),
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            hue: 0.0,
            signature: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
        }
    }

    fn toasts(rx: &std::sync::mpsc::Receiver<Event>) -> Vec<String> {
        rx.try_iter().filter_map(|e| if let Event::Toast(t) = e { Some(t) } else { None }).collect()
    }

    /// Stands in for the server. The first op to go "on the wire" runs `during` (something
    /// arriving mid-flight: an undo, a STATUS from the other connection), then every op
    /// succeeds, or fails with `fail`. STATUS answers `counts`.
    type MidFlight = Box<dyn FnOnce(&Shared) + Send>;

    struct FakeServer {
        shared: Arc<Shared>,
        during: Option<MidFlight>,
        fail: bool,
        ran: Vec<Op>,
        counts: (u32, u32),
        /// What a failing server answers.
        failure: &'static str,
        /// The Drafts mailbox: `(uid, Message-ID, raw)`.
        drafts: Vec<(u32, String, Vec<u8>)>,
        next_uid: u32,
        appends: usize,
        /// What each draft save was given: the copy on record to replace, and whether to look
        /// for strays.
        previous: Vec<(Option<(u32, u32)>, bool)>,
        /// Saves can't tell the new copy from the old ones (no APPENDUID, a search that misses):
        /// older copies are kept.
        unsure: bool,
        /// The next purge goes through on the server, but its answer is lost (the connection
        /// drops).
        cut_off: bool,
        /// Sends are refused as bigger than this ([`smtp::TooBig`]), which is learned.
        too_big: Option<u64>,
        /// Every mailbox is in generation `.0` (UIDVALIDITY), where messages are found again by
        /// Message-ID at these UIDs.
        generation: Option<(u32, Vec<(&'static str, u32)>)>,
        /// A failing server's connection drops instead (inside a literal: an I/O error).
        lost: bool,
        /// Where sent messages were filed, and whether the next filing's connection drops.
        filed: Vec<String>,
        filing_drops: bool,
        /// Filing in Sent is refused with this NO, until it's cleared.
        filing_refusal: Option<&'static str>,
        /// Snooze buckets made, in order, and a parent the server won't create.
        buckets: Vec<String>,
        refuses: Option<&'static str>,
    }

    impl FakeServer {
        fn new(shared: &Arc<Shared>, during: impl FnOnce(&Shared) + Send + 'static) -> FakeServer {
            FakeServer {
                shared: shared.clone(),
                during: Some(Box::new(during)),
                fail: false,
                failure: "code: Some(Unavailable), info: Some(\"try again later\")",
                ran: vec![],
                counts: (0, 0),
                drafts: vec![],
                next_uid: 1,
                appends: 0,
                previous: vec![],
                unsure: false,
                cut_off: false,
                too_big: None,
                generation: None,
                lost: false,
                filed: vec![],
                filing_drops: false,
                filing_refusal: None,
                buckets: vec![],
                refuses: None,
            }
        }

        /// Runs `during` if this is the first op on the wire; a failing server's NO.
        fn on_the_wire(&mut self) -> Result<()> {
            if let Some(during) = self.during.take() {
                during(&self.shared);
            }
            if self.fail && self.lost {
                return Err(anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "bytes remaining in stream")));
            }
            if self.fail { Err(anyhow::Error::new(async_imap::error::Error::No(self.failure.into()))) } else { Ok(()) }
        }

        /// The drafts on the server, `(uid, subject)`.
        fn drafts(&self) -> Vec<(u32, String)> {
            self.drafts.iter().map(|(uid, _, raw)| (*uid, crate::mime::parse(raw, true).unwrap().subject)).collect()
        }
    }

    impl OpRunner for FakeServer {
        fn run(&mut self, op: &Op, at: &At) -> impl std::future::Future<Output = Result<()>> + Send {
            // A mailbox in another generation than the op's: it doesn't run.
            if let (Some((validity, moved)), Some((mailbox, _))) = (&self.generation, op.located())
                && at.validity != *validity
            {
                let found = moved.iter().find(|(m, _)| *m == at.message_id).map(|(_, uid)| *uid);
                let e = imap::NewGeneration { mailbox: mailbox.to_owned(), validity: *validity, found };
                return futures::future::Either::Left(async move { Err(e.into()) });
            }
            self.ran.push(op.clone());
            let r = self.on_the_wire();
            futures::future::Either::Right(async move { r })
        }

        fn deliver(&mut self, op: &Op) -> impl std::future::Future<Output = Result<()>> + Send {
            let r = match (op, self.too_big) {
                // Refused before a byte goes, the size learned, as smtp::send does.
                (Op::Send { raw_b64, .. }, Some(limit)) => {
                    self.shared.learned_size("a", Some(limit));
                    Err(anyhow::Error::new(smtp::TooBig { size: raw_b64.len() * 3 / 4, limit: Some(limit) }))
                }
                _ => {
                    self.ran.push(op.clone());
                    self.on_the_wire()
                }
            };
            async move { r }
        }

        fn file_sent(&mut self, mailbox: &str, _: &[u8]) -> impl std::future::Future<Output = Result<()>> + Send {
            self.filed.push(mailbox.to_owned());
            let r = if std::mem::take(&mut self.filing_drops) {
                Err(anyhow::Error::new(imap::Lost("APPEND: the connection closed".into())))
            } else if let Some(no) = self.filing_refusal {
                Err(anyhow::Error::new(async_imap::error::Error::No(no.into())))
            } else {
                Ok(())
            };
            async move { r }
        }

        fn status(&mut self, _: &str) -> impl std::future::Future<Output = Result<(u32, u32)>> + Send {
            let counts = self.counts;
            async move { Ok(counts) }
        }

        fn create_bucket(&mut self, parent: &str, bucket: &str) -> impl std::future::Future<Output = Result<()>> + Send {
            let r = if self.refuses == Some(parent) {
                Err(anyhow::Error::new(imap::CantCreate { mailbox: parent.to_owned(), why: "NO [CANNOT] reserved".into() }))
            } else {
                if !self.buckets.iter().any(|b| b == bucket) {
                    self.buckets.push(bucket.to_owned());
                }
                Ok(())
            };
            async move { r }
        }

        fn replace_draft(&mut self, _: &str, raw: &[u8], message_id: &str, previous: Option<(u32, u32)>, maybe_strays: bool) -> impl std::future::Future<Output = Result<imap::Replaced>> + Send {
            self.previous.push((previous, maybe_strays));
            let r = self.on_the_wire().map(|()| {
                self.appends += 1;
                let uid = self.next_uid;
                self.next_uid += 1;
                let old: Vec<u32> = self.drafts.iter().filter(|(_, m, _)| m == message_id).map(|(u, _, _)| *u).collect();
                self.drafts.push((uid, message_id.to_owned(), raw.to_vec()));
                if self.unsure {
                    return imap::Replaced { copy: None, deleted: vec![], certain: false };
                }
                self.drafts.retain(|(u, m, _)| m != message_id || *u == uid);
                imap::Replaced { copy: Some((1, uid)), deleted: old, certain: true }
            });
            async move { r }
        }

        fn purge_draft(&mut self, _: &str, message_id: &str, known: Option<(u32, u32)>, search: bool) -> impl std::future::Future<Output = Result<Vec<u32>>> + Send {
            let r = self.on_the_wire().map(|()| {
                let goes = |u: u32, m: &str| known.is_some_and(|(_, k)| k == u) || (search && m == message_id);
                let gone: Vec<u32> = self.drafts.iter().filter(|(u, m, _)| goes(*u, m)).map(|(u, _, _)| *u).collect();
                self.drafts.retain(|(u, m, _)| !goes(*u, m));
                gone
            });
            let r = match r {
                Ok(_) if std::mem::take(&mut self.cut_off) => Err(anyhow::Error::new(imap::Lost("EXPUNGE: the connection closed before the server finished".into()))),
                r => r,
            };
            async move { r }
        }
    }

    fn undo_during(key: i64) -> impl FnOnce(&Shared) + Send + 'static {
        move |s: &Shared| undo_move(s, key).unwrap()
    }

    fn unread(shared: &Shared, account: &str) -> u32 {
        shared.with_writer(|st| st.mailboxes(account)).unwrap().iter().find(|m| m.role == Role::Inbox).unwrap().unread
    }

    // The undo window has just run out (the op is due) while the toast is still up.
    fn due_move(shared: &Shared, ids: Vec<MessageId>, key: i64) {
        move_and_hold(shared, ids, &MoveTarget::Archive, false, Some(key), now() - 10).unwrap();
    }

    #[test]
    fn a_held_move_is_due_exactly_when_its_window_and_grace_end() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        // Undone at 1000 with the 5 s window: due at 1006, after the second of grace.
        move_and_hold(&shared, vec![a], &MoveTarget::Archive, false, Some(3), 1000).unwrap();
        shared
            .with_writer(|st| {
                assert_eq!(st.next_op_due("a", 1000)?, Some(6), "the worker plans to wake at the end");
                assert!(st.claim_due("a", 1005)?.is_empty(), "a second before: not replayed");
                st.unclaim("a")?;
                assert_eq!(st.claim_due("a", 1006)?.len(), 1, "at the end: replayed");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_move_that_does_nothing_tells_the_ui() {
        let st = setup();
        let a = message(&st, "Archive", 1, "<1@x>", "", 0);
        let (shared, rx) = engine_with(st);
        // Undoable: acknowledged with no ops held (the app reloads and drops its Undo).
        move_and_hold(&shared, vec![a], &MoveTarget::Archive, false, Some(3), now()).unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::Held { undo: 3, ops: 0 })));
        // Undo turned off: a plain reload request.
        move_and_hold(&shared, vec![a], &MoveTarget::Archive, false, None, now()).unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::Changed { .. })));
    }

    #[test]
    fn flags_of_a_hidden_message_dont_change() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let b = message(&st, "INBOX", 2, "<2@x>", "", 0);
        st.set_counts("a", "INBOX", 2, 2).unwrap();
        let (shared, _rx) = engine_with(st);
        due_move(&shared, vec![a], 7);
        shared.with_writer(|st| st.claim_due("a", i64::MAX)).unwrap(); // on the wire
        undo_move(&shared, 7).unwrap(); // too late: A stays hidden
        // The UI reopened A optimistically and marked it read before learning that.
        for thread in [false, true] {
            assert!(set_flags(&shared, vec![a], flags::SEEN, 0, thread).unwrap().is_empty(), "thread: {thread}");
        }
        shared.with_writer(|st| {
            assert_eq!(st.summary(a)?.unwrap().flags & flags::SEEN, 0, "hidden A untouched");
            assert_eq!(st.pending_count()?, 1, "only the move is queued");
            let inbox = st.mailboxes("a")?.remove(0);
            assert_eq!((inbox.unread, inbox.total), (1, 1), "B is still unread and counted");
            assert!(st.summary(b)?.is_some());
            Ok(())
        })
        .unwrap();
        // Visible messages still change.
        assert_eq!(set_flags(&shared, vec![b], flags::SEEN, 0, false).unwrap(), HashSet::from(["a".to_owned()]));
    }

    #[tokio::test]
    async fn counts_are_right_after_a_status_races_the_replay() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        message(&st, "INBOX", 2, "<2@x>", "", 0);
        st.set_counts("a", "INBOX", 2, 2).unwrap();
        let (shared, _rx) = engine_with(st);
        due_move(&shared, vec![a], 7); // two unread, one archived: the badge says 1
        // While the move is on the wire the server carries it out, and a STATUS from the
        // foreground connection already reflects that, before op_done is recorded.
        let mut server = FakeServer::new(&shared, |s: &Shared| s.with_writer(|st| st.set_counts("a", "INBOX", 1, 1)).unwrap());
        server.counts = (1, 1);
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let inbox = shared.with_writer(|st| st.mailboxes("a")).unwrap().remove(0);
        assert_eq!((inbox.unread, inbox.total), (1, 1), "one unread message left, and the badge says so");
    }

    #[test]
    fn moving_a_message_already_on_its_way_changes_nothing() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let b = message(&st, "INBOX", 2, "<2@x>", "", 0);
        st.set_counts("a", "INBOX", 2, 2).unwrap();
        let (shared, rx) = engine_with(st);
        due_move(&shared, vec![a], 7);
        let original = shared.with_writer(|st| st.claim_due("a", i64::MAX)).unwrap()[0].id; // on the wire
        undo_move(&shared, 7).unwrap(); // too late; the UI put A's row back optimistically
        let _ = rx.try_iter().count();
        due_move(&shared, vec![a], 8); // and A is archived again before the reload
        assert!(rx.try_iter().any(|e| matches!(e, Event::Held { undo: 8, ops: 0 })), "the UI learns there's nothing to undo");
        shared.with_writer(|st| {
            assert_eq!(st.pending_count()?, 1, "no second op for a message that's already moving");
            assert_eq!(st.mailboxes("a")?[0].unread, 1, "unread counted down once");
            assert_eq!(st.undo_batch(8)?, crate::store::Undone::default(), "nothing to undo");
            st.op_done(original)?; // the original move lands
            assert!(st.summary(a)?.is_none(), "no ghost left in the inbox");
            assert_eq!(visible(st, "INBOX"), vec![b]);
            assert_eq!(st.mailboxes("a")?[0].unread, 1, "b still counted");
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test]
    async fn undo_while_the_move_is_on_the_wire_is_too_late() {
        let st = setup();
        let id = message(&st, "INBOX", 1, "<1@x>", "", 0);
        st.set_counts("a", "INBOX", 1, 1).unwrap();
        let (shared, rx) = engine_with(st);
        due_move(&shared, vec![id], 7);
        let mut server = FakeServer::new(&shared, undo_during(7));
        assert!(replay_with(&shared, &account("a"), &mut server).await.unwrap());
        assert_eq!(server.ran.len(), 1, "the server moved it");
        shared.with_writer(|st| {
            assert!(st.summary(id)?.is_none(), "so it's gone locally too");
            assert_eq!(st.pending_count()?, 0);
            Ok(())
        })
        .unwrap();
        assert_eq!(unread(&shared, "a"), 0, "no phantom unread");
        assert!(toasts(&rx).iter().any(|t| t.starts_with("Too late")), "and the user is told");
    }

    #[tokio::test]
    async fn undo_mid_flight_then_failure_leaves_the_move_queued() {
        let st = setup();
        let id = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let (shared, rx) = engine_with(st);
        due_move(&shared, vec![id], 7);
        let mut server = FakeServer { fail: true, ..FakeServer::new(&shared, undo_during(7)) };
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        shared.with_writer(|st| {
            let queued = st.ops("a", i64::MAX)?;
            assert_eq!(queued.len(), 1, "still on its way: retried later, not lost");
            assert_eq!(queued[0].attempts, 1);
            assert!(st.list(&View::Mailbox { account: "a".into(), mailbox: "INBOX".into() }, &Query::default(), false, 10, &HashSet::new())?.is_empty(), "still hidden");
            Ok(())
        })
        .unwrap();
        assert!(toasts(&rx).iter().any(|t| t.starts_with("Too late")));
    }

    #[tokio::test]
    async fn undo_mid_replay_takes_back_only_what_hasnt_started() {
        let st = setup();
        let a = message_in(&st, "a", "INBOX", 1, "<1@x>", "", 0);
        let b = message_in(&st, "b", "INBOX", 1, "<2@x>", "", 0);
        let (shared, rx) = engine_with(st);
        due_move(&shared, vec![a, b], 7); // multi-select in the unified inbox
        let mut server = FakeServer::new(&shared, undo_during(7));
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        shared.with_writer(|st| {
            assert!(st.summary(a)?.is_none(), "a's move was on the wire: done");
            let b_inbox = st.list(&View::Mailbox { account: "b".into(), mailbox: "INBOX".into() }, &Query::default(), false, 10, &HashSet::new())?;
            assert_eq!(b_inbox.iter().map(|s| s.id).collect::<Vec<_>>(), vec![b], "b's hadn't started: undone");
            assert!(st.ops("b", i64::MAX)?.is_empty());
            Ok(())
        })
        .unwrap();
        assert!(toasts(&rx).iter().any(|t| t.starts_with("Partly undone")));
    }

    fn visible(st: &Store, mailbox: &str) -> Vec<MessageId> {
        let view = View::Mailbox { account: "a".into(), mailbox: mailbox.into() };
        st.list(&view, &Query::default(), false, 100, &HashSet::new()).unwrap().into_iter().map(|s| s.id).collect()
    }

    #[test]
    fn archiving_a_conversation_holds_one_undoable_batch() {
        let st = setup();
        let first = message(&st, "INBOX", 1, "<1@x>", "", flags::SEEN);
        let reply = message(&st, "INBOX", 2, "<2@x>", "<1@x>", 0);
        let other = message(&st, "INBOX", 3, "<3@x>", "", 0);
        st.set_counts("a", "INBOX", 2, 3).unwrap();
        let ids = {
            let mut ids = st.thread_ids(reply, true).unwrap();
            ids.push(other); // multi-select: the conversation plus another message
            ids
        };
        let moved = move_local(&st, &ids, &MoveTarget::Archive, Some((1_000, 42))).unwrap();
        assert_eq!(moved.touched, HashSet::from(["a".to_owned()]));
        assert!(moved.problems.is_empty());
        assert_eq!(moved.held, 3);
        assert!(visible(&st, "INBOX").is_empty());
        assert_eq!((st.mailboxes("a").unwrap()[0].unread, st.mailboxes("a").unwrap()[0].total), (0, 0));
        assert!(st.ops("a", 999).unwrap().is_empty(), "held for the window");
        assert_eq!(st.ops("a", 1_000).unwrap().len(), 3);

        assert_eq!(st.undo_batch(42).unwrap().accounts, vec!["a".to_owned()]);
        let mut back = visible(&st, "INBOX");
        back.sort();
        assert_eq!(back, vec![first, reply, other], "every message back, same ids");
        assert_eq!((st.mailboxes("a").unwrap()[0].unread, st.mailboxes("a").unwrap()[0].total), (2, 3), "counts back");
        assert_eq!(st.pending_count().unwrap(), 0, "nothing reaches the server");
        let thread = st.thread(reply, true).unwrap();
        assert_eq!(thread.len(), 2, "the conversation opens whole again");
        assert!(thread.iter().all(|m| m.body.as_ref().is_some_and(|b| b.text.as_deref().is_some_and(|t| t.contains("hi")))), "bodies kept");
    }

    #[test]
    fn spam_and_trash_are_undoable_but_deleting_from_trash_is_not() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let b = message(&st, "INBOX", 2, "<2@x>", "", 0);
        let gone = message(&st, "Trash", 3, "<3@x>", "", 0);
        move_local(&st, &[a], &MoveTarget::Junk, Some((1_000, 1))).unwrap();
        move_local(&st, &[b], &MoveTarget::Trash, Some((1_000, 2))).unwrap();
        move_local(&st, &[gone], &MoveTarget::Trash, Some((1_000, 3))).unwrap();
        let due: Vec<Op> = st.ops("a", 0).unwrap().into_iter().map(|q| q.op).collect();
        assert_eq!(due, vec![Op::Expunge { mailbox: "Trash".into(), uid: 3 }], "expunge isn't held");
        assert!(st.summary(gone).unwrap().is_none());
        assert_eq!(st.undo_batch(3).unwrap().released, 0);
        st.undo_batch(1).unwrap();
        st.undo_batch(2).unwrap();
        let mut back = visible(&st, "INBOX");
        back.sort();
        assert_eq!(back, vec![a, b]);
    }

    #[test]
    fn without_an_undo_key_moves_replay_at_once() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        move_local(&st, &[a], &MoveTarget::Archive, None).unwrap();
        let due = st.ops("a", 0).unwrap();
        assert_eq!(due.len(), 1);
        assert!(visible(&st, "INBOX").is_empty());
        st.op_done(due[0].id).unwrap();
        assert!(st.summary(a).unwrap().is_none(), "the local copy goes once the server has it");
    }

    #[tokio::test]
    async fn changes_queued_before_a_reset_follow_their_message_or_are_dropped() {
        let st = setup();
        let archived = message(&st, "INBOX", 1, "<archived@x>", "", 0);
        let gone = message(&st, "INBOX", 2, "<gone@x>", "", 0);
        let read = message(&st, "INBOX", 3, "<read@x>", "", 0);
        st.set_uidvalidity("a", "INBOX", 7).unwrap();
        let (shared, rx) = engine_with(st);
        // Archived, archived, marked read: queued against generation 7.
        due_move(&shared, vec![archived], 1);
        due_move(&shared, vec![gone], 2);
        set_flags(&shared, vec![read], flags::SEEN, 0, false).unwrap();
        let at: Vec<At> = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().map(|q| q.at).collect();
        let at_7 = |m: &str| At { validity: 7, message_id: m.into() };
        assert_eq!(at, [at_7("<archived@x>"), at_7("<gone@x>"), at_7("<read@x>")]);
        // The server reset INBOX (generation 8): two of the messages are found again, at new
        // UIDs; the other isn't there.
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.generation = Some((8, vec![("<archived@x>", 5), ("<read@x>", 6)]));
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(
            server.ran,
            [
                Op::Move { mailbox: "INBOX".into(), uid: 5, dest: "Archive".into() },
                Op::SetFlags { mailbox: "INBOX".into(), uid: 6, add: flags::SEEN, remove: 0 },
            ],
            "each by its new UID; nothing by an old one"
        );
        shared
            .with_writer(|st| {
                assert!(st.ops("a", i64::MAX)?.is_empty(), "the dropped one too");
                assert_eq!(visible(st, "INBOX"), vec![read, gone], "the one that couldn't follow its message shows again");
                Ok(())
            })
            .unwrap();
        assert!(rx.try_iter().any(|e| matches!(&e, Event::Error(m) if m.contains("was dropped"))), "and the user is told");
    }

    /// Puts an op straight in the queue, as an older Envoy left it: no generation, `message_id`
    /// as given (the old Envoy recorded none).
    fn legacy_op(shared: &Shared, op: &Op, message_id: &str) -> i64 {
        shared
            .with_writer(|st| {
                let id = st.enqueue("a", op)?;
                st.conn_for_tests().execute("UPDATE ops SET uidvalidity = 0, message_id = ?2 WHERE id = ?1", rusqlite::params![id, message_id])?;
                Ok(id)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn a_lost_connection_never_counts_toward_giving_up() {
        let st = setup();
        let id = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        due_move(&shared, vec![id], 7);
        // The connection drops inside a literal, again and again (a flaky network): async-imap
        // says "bytes remaining in stream". The move must wait it out, however long.
        for _ in 0..8 {
            let mut server = FakeServer { fail: true, ..FakeServer::new(&shared, |_: &Shared| {}) };
            server.lost = true;
            assert!(replay_with(&shared, &account("a"), &mut server).await.is_err(), "the pass stops, to reconnect");
            assert_eq!(server.ran.len(), 1);
            shared.with_writer(|st| Ok(st.conn_for_tests().execute("UPDATE ops SET not_before = 0", [])?)).unwrap();
        }
        let queued = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap();
        assert_eq!((queued.len(), queued[0].attempts), (1, 0), "still queued, no attempt counted");
        assert!(visible(&shared.writer.lock().unwrap(), "INBOX").is_empty(), "still hidden");
    }

    #[tokio::test]
    async fn a_message_out_before_the_connection_drops_is_never_sent_again() {
        let (shared, _rx) = engine_with(setup());
        let raw_b64 = base64::engine::general_purpose::STANDARD.encode(b"Subject: hi\r\n\r\nhello\r\n");
        let send = Op::Send { raw_b64, from: "me@x.com".into(), to: vec!["you@x.com".into()], sent_mailbox: Some("Sent".into()), reply_of: None };
        shared.with_writer(|st| st.enqueue("a", &send)).unwrap();
        // SMTP takes it; the IMAP connection drops while it's filed in Sent.
        let mut server = FakeServer { filing_drops: true, ..FakeServer::new(&shared, |_: &Shared| {}) };
        assert!(replay_with(&shared, &account("a"), &mut server).await.is_err(), "the pass stops, to reconnect");
        let sends = |server: &FakeServer| server.ran.iter().filter(|op| matches!(op, Op::Send { .. })).count();
        assert_eq!(sends(&server), 1);
        let queued = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap();
        assert!(matches!(&queued[..], [q] if matches!(&q.op, Op::Delivered { sent_mailbox: Some(_), .. }) && q.attempts == 0), "{:?}", queued.iter().map(|q| (&q.op, q.attempts)).collect::<Vec<_>>());
        // On the new connection: filed, not sent again.
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(sends(&server), 1, "sent once: {:?}", server.ran);
        assert_eq!(server.filed, ["Sent", "Sent"]);
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty());
    }

    /// Queues a send to be filed in Sent, due now.
    fn queue_reply(shared: &Shared) {
        let raw_b64 = base64::engine::general_purpose::STANDARD.encode(b"Subject: hi\r\n\r\nhello\r\n");
        let send = Op::Send { raw_b64, from: "me@x.com".into(), to: vec!["you@x.com".into()], sent_mailbox: Some("Sent".into()), reply_of: None };
        shared.with_writer(|st| st.enqueue("a", &send)).unwrap();
    }

    fn sends(server: &FakeServer) -> usize {
        server.ran.iter().filter(|op| matches!(op, Op::Send { .. })).count()
    }

    fn make_due(shared: &Shared) {
        shared.with_writer(|st| Ok(st.conn_for_tests().execute("UPDATE ops SET not_before = 0", [])?)).unwrap();
    }

    #[tokio::test]
    async fn a_message_out_is_never_sent_again_though_recording_it_fails() {
        let (shared, _rx) = engine_with(setup());
        queue_reply(&shared);
        // SMTP takes it; recording that fails for a while (the disk is full, say).
        let trigger = "CREATE TRIGGER no_delivered BEFORE UPDATE OF op ON ops WHEN json_extract(NEW.op, '$.kind') = 'delivered'
                       BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;";
        shared.with_writer(|st| Ok(st.conn_for_tests().execute_batch(trigger)?)).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        let clears = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            shared.with_writer(|st| Ok(st.conn_for_tests().execute_batch("DROP TRIGGER no_delivered")?)).unwrap();
        };
        let a = account("a");
        let (first, ()) = tokio::join!(replay_with(&shared, &a, &mut server), clears);
        let _ = first;
        // However replay goes on from there, the message isn't sent again.
        for _ in 0..3 {
            make_due(&shared);
            let _ = replay_with(&shared, &account("a"), &mut server).await;
        }
        assert_eq!(sends(&server), 1, "{:?}", server.ran);
        assert_eq!(server.filed, ["Sent"]);
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_sent_copy_the_server_cant_take_now_waits_to_be_filed() {
        let (shared, _rx) = engine_with(setup());
        queue_reply(&shared);
        let mut server = FakeServer { filing_refusal: Some("code: Some(Unavailable), info: Some(\"[UNAVAILABLE] try again later\")"), ..FakeServer::new(&shared, |_: &Shared| {}) };
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let queued = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap();
        assert!(matches!(&queued[..], [q] if matches!(&q.op, Op::Delivered { sent_mailbox: Some(_), .. })), "the copy waits, Delivered: {:?}", queued.iter().map(|q| &q.op).collect::<Vec<_>>());
        // Not now, again and again: it keeps waiting, with backoff, however long.
        for _ in 0..7 {
            make_due(&shared);
            replay_with(&shared, &account("a"), &mut server).await.unwrap();
        }
        let queued = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap();
        assert_eq!((queued.len(), queued[0].attempts), (1, 8));
        server.filing_refusal = None;
        make_due(&shared);
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!((sends(&server), server.filed.len()), (1, 9), "filed at last, never sent again");
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_sent_copy_the_server_refuses_for_good_is_kept_here() {
        let (shared, rx) = engine_with(setup());
        queue_reply(&shared);
        let mut server = FakeServer { filing_refusal: Some("code: Some(OverQuota), info: Some(\"[OVERQUOTA] mailbox full\")"), ..FakeServer::new(&shared, |_: &Shared| {}) };
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let kept = shared.with_writer(|st| st.unfiled("a")).unwrap();
        assert!(matches!(&kept[..], [(mailbox, raw, why)] if mailbox == "Sent" && raw.as_slice() == b"Subject: hi\r\n\r\nhello\r\n" && why.contains("OVERQUOTA")), "{kept:?}");
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty());
        assert_eq!(sends(&server), 1);
        let notices: Vec<String> = rx.try_iter().filter_map(|e| if let Event::Error(m) = e { Some(m) } else { None }).collect();
        assert!(matches!(&notices[..], [m] if m.contains("was sent") && m.contains("kept on this computer")), "one notice: {notices:?}");
    }

    #[test]
    fn changes_from_before_the_update_that_cant_be_proven_go_with_one_notice() {
        let st = setup();
        let held = message(&st, "INBOX", 1, "<held@x>", "", 0);
        st.set_uidvalidity("a", "INBOX", 8).unwrap();
        let (shared, rx) = engine_with(st);
        let mv = legacy_op(&shared, &Op::Move { mailbox: "INBOX".into(), uid: 1, dest: "Archive".into() }, "");
        shared.with_writer(|st| st.hide_local(held, mv)).unwrap();
        legacy_op(&shared, &Op::SetFlags { mailbox: "INBOX".into(), uid: 2, add: flags::SEEN, remove: 0 }, "");
        legacy_op(&shared, &Op::Copy { mailbox: "INBOX".into(), uid: 3, dest: "Work".into() }, "");
        // An expunge goes even with a Message-ID: deleting needs proof.
        legacy_op(&shared, &Op::Expunge { mailbox: "INBOX".into(), uid: 4 }, "<doomed@x>");
        // These stay: a move that can be found again, a send, a draft upload.
        let findable = legacy_op(&shared, &Op::Move { mailbox: "INBOX".into(), uid: 5, dest: "Archive".into() }, "<findable@x>");
        let send = legacy_op(&shared, &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: Some(ReplyOf::Legacy("INBOX".into(), 6)) }, "");
        let draft = legacy_op(&shared, &Op::SaveDraft { draft: 1 }, "");
        drop_unprovable_ops(&shared);
        let left: Vec<i64> = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().map(|q| q.id).collect();
        assert_eq!(left, [findable, send, draft]);
        assert_eq!(shared.with_writer(|st| Ok(visible(st, "INBOX"))).unwrap(), [held], "what the dropped move hid shows again");
        let notices: Vec<String> = rx.try_iter().filter_map(|e| if let Event::Error(m) = e { Some(m) } else { None }).collect();
        assert_eq!(notices.len(), 1, "one notice, not one per change: {notices:?}");
        assert!(notices[0].starts_with("4 queued changes from before the update couldn't be applied safely"), "{}", notices[0]);
        // The next start finds nothing more to say.
        drop_unprovable_ops(&shared);
        assert!(rx.try_iter().next().is_none());
    }

    #[tokio::test]
    async fn an_expunge_from_an_unknown_generation_never_runs() {
        let st = setup();
        st.set_uidvalidity("a", "INBOX", 8).unwrap();
        let (shared, _rx) = engine_with(st);
        // Somehow still queued (the sweep keeps none): even found by its Message-ID, it doesn't run.
        legacy_op(&shared, &Op::Expunge { mailbox: "INBOX".into(), uid: 1 }, "<doomed@x>");
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.generation = Some((8, vec![("<doomed@x>", 5)]));
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.ran.is_empty(), "{:?}", server.ran);
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty(), "dropped");
    }

    fn listed(name: &str, role: Role) -> imap::ListedMailbox {
        imap::ListedMailbox { name: name.into(), display: name.into(), role, delimiter: "/".into(), selectable: true, native_snoozed: false }
    }

    #[test]
    fn only_a_listing_with_the_inbox_forgets_mailboxes() {
        let st = setup();
        let kept = message(&st, "Archive", 1, "<1@x>", "", 0);
        // Every server lists INBOX: a listing without it isn't the whole list, and mustn't
        // forget the mailboxes it leaves out, or their cached mail.
        store_listing(&st, "a", &[listed("Work", Role::Other)]).unwrap();
        let names: Vec<String> = st.mailboxes("a").unwrap().into_iter().map(|m| m.name).collect();
        assert!(names.contains(&"Archive".to_owned()) && names.contains(&"Work".to_owned()), "{names:?}");
        assert!(st.summary(kept).unwrap().is_some(), "its mail kept");
        // A whole listing still forgets what's gone.
        store_listing(&st, "a", &[listed("INBOX", Role::Inbox), listed("Work", Role::Other)]).unwrap();
        let names: Vec<String> = st.mailboxes("a").unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["INBOX", "Work"]);
        assert!(st.summary(kept).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_label_is_a_held_undoable_copy_that_leaves_the_message() {
        let st = setup();
        st.upsert_mailbox("a", "Work", "Work", Role::Other, "/", true).unwrap();
        let first = message(&st, "INBOX", 1, "<1@x>", "", flags::SEEN);
        let reply = message(&st, "INBOX", 2, "<2@x>", "<1@x>", 0);
        let other = message(&st, "INBOX", 3, "<3@x>", "", 0);
        let (shared, rx) = engine_with(st);
        // A conversation and another message, labeled in one go.
        copy_and_hold(&shared, vec![reply, other], "Work", true, Some(9), 1000).unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::Held { undo: 9, ops: 3 })), "every message held under one key");
        let inbox = shared.with_writer(|st| Ok(visible(st, "INBOX"))).unwrap();
        assert_eq!(inbox.len(), 3, "nothing leaves the inbox");
        assert!(shared.with_writer(|st| st.ops("a", 1005)).unwrap().is_empty(), "held for the window and its second of grace");
        // Undone in the window: the server never sees it.
        undo_move(&shared, 9).unwrap();
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 0);
        assert!(toasts(&rx).is_empty(), "no 'too late'");
        // Labeled again, and left: the copies reach the server, the messages stay put.
        copy_and_hold(&shared, vec![first], "Work", false, Some(10), now() - 10).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        assert!(replay_with(&shared, &account("a"), &mut server).await.unwrap());
        assert_eq!(server.ran, vec![Op::Copy { mailbox: "INBOX".into(), uid: 1, dest: "Work".into() }]);
        assert_eq!(shared.with_writer(|st| Ok(visible(st, "INBOX"))).unwrap().len(), 3);
        // Already in the label's mailbox: nothing to do, nothing to undo.
        let in_work = message(&shared.writer.lock().unwrap(), "Work", 4, "<4@x>", "", 0);
        copy_and_hold(&shared, vec![in_work], "Work", false, Some(11), now()).unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::Held { undo: 11, ops: 0 })));
    }

    #[tokio::test]
    async fn read_marks_recount_their_mailbox_once_on_the_server() {
        let st = setup();
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        message(&st, "INBOX", 2, "<2@x>", "", 0);
        st.set_counts("a", "INBOX", 2, 2).unwrap();
        let (shared, _rx) = engine_with(st);
        set_flags(&shared, vec![a], flags::SEEN, 0, false).unwrap();
        assert_eq!(unread(&shared, "a"), 1, "counted down at once");
        // A STATUS from the other connection lands before the mark reaches the server.
        shared.with_writer(|st| st.set_counts("a", "INBOX", 2, 2)).unwrap();
        assert_eq!(unread(&shared, "a"), 1, "the queued mark still counts");
        // The mark replays; the recount after it reads the server's new count.
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.counts = (1, 2);
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(unread(&shared, "a"), 1);
    }

    /// A foreground connection whose sessions are numbered as they're opened. `stale`: sessions
    /// below it were dropped by the server (jobs and probes on them fail as a lost connection
    /// would); `refuse`: the server answers jobs with NO; `offline`: connecting fails.
    #[derive(Default)]
    struct FakeForeground {
        opened: u32,
        stale: u32,
        refuse: bool,
        offline: bool,
        ran_on: Vec<u32>,
        probed: Vec<u32>,
    }

    impl Foreground for FakeForeground {
        type Session = u32;

        async fn connect(&mut self) -> Result<u32> {
            anyhow::ensure!(!self.offline, "Couldn't reach imap.x.com:993");
            self.opened += 1;
            Ok(self.opened)
        }

        async fn probe(&mut self, session: &mut u32) -> Result<()> {
            self.probed.push(*session);
            if *session < self.stale {
                return Err(anyhow::Error::new(imap::Lost("NOOP: timed out".into())));
            }
            Ok(())
        }

        async fn run(&mut self, session: &mut u32, _: &Job) -> Result<()> {
            self.ran_on.push(*session);
            if self.refuse {
                return Err(anyhow::Error::new(async_imap::error::Error::No("no such mailbox".into())));
            }
            if *session < self.stale {
                return Err(anyhow::Error::new(imap::Lost("SELECT timed out".into())));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_dropped_foreground_connection_is_replaced_before_falling_back() {
        // The open connection was closed by the server while idle: the job runs on a new one,
        // which is kept for the next.
        let mut fg = FakeForeground { opened: 1, stale: 2, ..Default::default() };
        let mut session = Some(1);
        on_foreground(&mut fg, &mut session, RECENT, &Job::Sync).await.unwrap();
        assert_eq!((fg.ran_on.as_slice(), session), (&[1, 2][..], Some(2)));
        // Once only: a job that fails on a fresh connection goes to the background one.
        let mut fg = FakeForeground { stale: 9, ..Default::default() };
        let mut session = None;
        assert!(on_foreground(&mut fg, &mut session, RECENT, &Job::Sync).await.is_err());
        assert_eq!((fg.ran_on.as_slice(), session), (&[1][..], None));
        // A server that answers NO isn't a lost connection: no second try.
        let mut fg = FakeForeground { opened: 1, refuse: true, ..Default::default() };
        let mut session = Some(1);
        assert!(on_foreground(&mut fg, &mut session, RECENT, &Job::Sync).await.is_err());
        assert_eq!((fg.ran_on.as_slice(), fg.opened, session), (&[1][..], 1, None));
        // Offline: the dropped connection can't be replaced either.
        let mut fg = FakeForeground { opened: 1, stale: 2, offline: true, ..Default::default() };
        let mut session = Some(1);
        assert!(on_foreground(&mut fg, &mut session, RECENT, &Job::Sync).await.is_err());
        assert_eq!(session, None);
    }

    /// Since a foreground connection's last job: recently, and long enough to probe it first.
    const RECENT: Duration = Duration::from_secs(5);
    const LONG: Duration = Duration::from_secs(61);

    #[tokio::test]
    async fn only_a_foreground_connection_left_idle_is_probed() {
        // In use a moment ago: no probe, the job goes straight on.
        let mut fg = FakeForeground { opened: 1, ..Default::default() };
        let mut session = Some(1);
        on_foreground(&mut fg, &mut session, RECENT, &Job::Sync).await.unwrap();
        assert_eq!((fg.probed.as_slice(), fg.ran_on.as_slice()), (&[][..], &[1][..]));
        // Idle a while, and still answering: probed, then used.
        let mut fg = FakeForeground { opened: 1, ..Default::default() };
        let mut session = Some(1);
        on_foreground(&mut fg, &mut session, LONG, &Job::Sync).await.unwrap();
        assert_eq!((fg.probed.as_slice(), fg.ran_on.as_slice(), session), (&[1][..], &[1][..], Some(1)));
        // Idle a while and silent: the job never runs on it, only on a new one, which is kept.
        let mut fg = FakeForeground { opened: 1, stale: 2, ..Default::default() };
        let mut session = Some(1);
        on_foreground(&mut fg, &mut session, LONG, &Job::Sync).await.unwrap();
        assert_eq!((fg.probed.as_slice(), fg.ran_on.as_slice(), session), (&[1][..], &[2][..], Some(2)));
        // No connection yet: nothing to probe.
        let mut fg = FakeForeground::default();
        let mut session = None;
        on_foreground(&mut fg, &mut session, LONG, &Job::Sync).await.unwrap();
        assert_eq!((fg.probed.as_slice(), session), (&[][..], Some(1)));
    }

    #[test]
    fn idle_time_counts_the_computer_asleep() {
        // The monotonic clock stood still while asleep; the wall clock went on.
        let slept = Used { at: Instant::now(), wall: std::time::SystemTime::now() - Duration::from_secs(3600) };
        assert!(slept.idle() >= Duration::from_secs(3600));
        // A wall clock set back doesn't hide the time that passed.
        let set_back = Used { at: Instant::now() - Duration::from_secs(90), wall: std::time::SystemTime::now() + Duration::from_secs(3600) };
        assert!(set_back.idle() >= Duration::from_secs(90));
    }

    /// The foreground's connections as the wire has them: each a session with the fake server,
    /// the first answering nothing at all once logged in (dropped without a word), the rest
    /// answering a SELECT. A job is a SELECT.
    struct WireForeground {
        opened: usize,
        logs: Vec<imap::fake::Log>,
    }

    impl Foreground for WireForeground {
        type Session = imap::Connection<tokio::io::DuplexStream>;

        async fn connect(&mut self) -> Result<Self::Session> {
            let dead = self.opened == 0;
            self.opened += 1;
            let (s, log) = imap::fake::session(move |_| Some(if dead { imap::fake::SILENT.to_owned() } else { imap::fake::selected(7, 10) })).await;
            self.logs.push(log);
            Ok(s)
        }

        async fn probe(&mut self, session: &mut Self::Session) -> Result<()> {
            imap::probe(session).await
        }

        async fn run(&mut self, session: &mut Self::Session, _: &Job) -> Result<()> {
            imap::select(session, "INBOX").await
        }
    }

    #[tokio::test]
    async fn a_silently_dead_foreground_connection_is_replaced_in_seconds() {
        let mut fg = WireForeground { opened: 0, logs: Vec::new() };
        let mut session = Some(fg.connect().await.unwrap());
        let t = Instant::now();
        on_foreground(&mut fg, &mut session, LONG, &Job::Sync).await.unwrap();
        // Done on a new connection in the probe's few seconds, not the SELECT's 30.
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
        assert_eq!(fg.opened, 2);
        let commands = |i: usize| fg.logs[i].lock().unwrap().iter().filter(|c| !c.starts_with("LOGIN")).cloned().collect::<Vec<_>>();
        assert_eq!((commands(0), commands(1)), (vec!["NOOP".to_owned()], vec!["SELECT \"INBOX\"".to_owned()]));
        assert!(session.is_some(), "the new connection is kept for the next job");
    }

    #[tokio::test]
    async fn a_watched_mailbox_is_synced_when_its_status_moved_on_since_its_sync() {
        let (shared, _rx) = engine_with(setup());
        let synced = |m: &str, exists, uid_next| shared.synced("a", m, imap::Contents { exists, uid_next: Some(uid_next), uid_validity: Some(7) });
        for m in ["Junk", "Archive", "Gone", "Remade"] {
            synced(m, 3, 10);
        }
        let (mut session, log) = imap::fake::session(|c| {
            Some(match c {
                // Filed into elsewhere, then nothing else.
                "STATUS \"Junk\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => "* STATUS \"Junk\" (MESSAGES 4 UNSEEN 1 UIDNEXT 11 UIDVALIDITY 7)\r\nTAG OK done\r\n".into(),
                "STATUS \"Archive\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => "* STATUS \"Archive\" (MESSAGES 3 UNSEEN 0 UIDNEXT 10 UIDVALIDITY 7)\r\nTAG OK done\r\n".into(),
                // Deleted and made again elsewhere, as full and with as many UIDs used.
                "STATUS \"Remade\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => "* STATUS \"Remade\" (MESSAGES 3 UNSEEN 0 UIDNEXT 10 UIDVALIDITY 8)\r\nTAG OK done\r\n".into(),
                "STATUS \"Gone\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)" => "TAG NO no such mailbox\r\n".into(),
                _ => "* STATUS \"Receipts\" (MESSAGES 0 UNSEEN 0 UIDNEXT 1 UIDVALIDITY 7)\r\nTAG OK done\r\n".into(),
            })
        })
        .await;
        let watched = ["Junk", "Archive", "Remade", "Gone", "Receipts"].map(String::from);
        // Receipts has no sync on record: it's synced too.
        assert_eq!(changed_since_synced(&shared, "a", &mut session, &watched).await.unwrap(), ["Junk", "Remade", "Receipts"]);
        assert_eq!(log.lock().unwrap().len(), 1 + watched.len(), "one STATUS each, after the LOGIN");
        // Synced since: nothing to do.
        synced("Junk", 4, 11);
        shared.synced("a", "Remade", imap::Contents { exists: 3, uid_next: Some(10), uid_validity: Some(8) });
        shared.synced("a", "Receipts", imap::Contents { exists: 0, uid_next: Some(1), uid_validity: Some(7) });
        assert!(changed_since_synced(&shared, "a", &mut session, &watched).await.unwrap().is_empty());
        // A connection lost while looking ends the round, as any lost command does.
        let (mut session, _) = imap::fake::session(|_| Some(imap::fake::HANG_UP.to_owned())).await;
        assert!(changed_since_synced(&shared, "a", &mut session, &watched).await.is_err_and(|e| imap::is_lost(&e)));
    }

    // ── drafts ──

    fn draft(subject: &str) -> Draft {
        Draft { account: "a".into(), to: "Bo <bo@x.com>".into(), subject: subject.into(), body: format!("About {subject}."), ..Draft::default() }
    }

    /// A save from the draft's first lifecycle, stamped `rev`.
    fn at(rev: i64) -> DraftRev {
        DraftRev { lifecycle: 0, rev }
    }

    /// Saves the composer's content as the app does while typing, `rev` ordering the saves.
    /// The save is dated in the past, so its upload is due now.
    fn type_draft(shared: &Shared, id: i64, rev: i64, subject: &str) {
        shared.with_writer(|st| drafts::save(st, id, &draft(subject), true, at(rev), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
    }

    fn drafts_engine() -> (Arc<Shared>, std::sync::mpsc::Receiver<Event>) {
        let (shared, rx) = engine_with(setup());
        *shared.accounts.write().unwrap() = vec![account("a"), account("b")];
        (shared, rx)
    }

    fn drafts_list(shared: &Shared, query: &str) -> Vec<Summary> {
        let view = View::Role { account: None, role: Role::Drafts };
        let q = Query::parse(query);
        shared.with_writer(|st| drafts::merge(st, &view, &q, 100, st.list(&view, &q, false, 100, &HashSet::new())?, |a| shared.sender(a))).unwrap()
    }

    /// Caches the server's copy of a draft, as a sync of the Drafts mailbox would.
    fn sync_copy(shared: &Shared, server: &FakeServer, uid: u32) -> MessageId {
        let (_, _, raw) = server.drafts.iter().find(|(u, _, _)| *u == uid).unwrap();
        let p = crate::mime::parse(raw, false).unwrap();
        shared.with_writer(|st| {
            let id = st.upsert_header("a", "Drafts", uid, flags::DRAFT | flags::SEEN, 10, 0, &p)?;
            st.set_body(id, raw, &p)?;
            Ok(id)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn rapid_edits_make_one_server_update() {
        let (shared, _rx) = drafts_engine();
        for (rev, subject) in [(1, "v1"), (2, "v2"), (3, "v3")] {
            type_draft(&shared, 7, rev, subject);
        }
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 1, "one upload queued for three saves");
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.appends, 1);
        assert_eq!(server.drafts(), vec![(1, "v3".to_owned())], "the newest version, once");
        assert!(!shared.with_writer(|st| st.draft_needs_upload(7)).unwrap());
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 0);
    }

    #[tokio::test]
    async fn an_edit_replaces_the_server_copy() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "first");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let old = sync_copy(&shared, &server, 1);
        type_draft(&shared, 7, 2, "second");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(2, "second".to_owned())], "appended anew, the old copy deleted");
        assert!(shared.with_writer(|st| st.summary(old)).unwrap().is_none(), "and gone from the cache");
    }

    #[tokio::test]
    async fn each_save_replaces_the_copy_on_record() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "v1");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        type_draft(&shared, 7, 2, "v2");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.previous, vec![(None, false), (Some((1, 1)), false)], "the second save replaces the first's copy, by UID");
        // A save that can't tell its copy from older ones keeps them all...
        server.unsure = true;
        type_draft(&shared, 7, 3, "v3");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts().len(), 2, "kept rather than risked");
        // ...and the next one, with no copy on record, looks for strays.
        server.unsure = false;
        type_draft(&shared, 7, 4, "v4");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.previous.last(), Some(&(None, true)));
        assert_eq!(server.drafts(), vec![(4, "v4".to_owned())]);
    }

    #[tokio::test]
    async fn an_upload_finishing_after_the_account_changed_is_not_trusted() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "from a");
        // The user picks another From while a's upload is on the wire.
        let mut a = FakeServer::new(&shared, |s: &Shared| {
            let moved = Draft { account: "b".into(), ..draft("from b") };
            s.with_writer(|st| drafts::save(st, 7, &moved, true, at(2), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
        });
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        // Both servers number their Drafts alike: a's UID must mean nothing on b.
        let mut b = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("b"), &mut b).await.unwrap();
        assert_eq!(b.previous.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![None], "b's first save replaces nothing of a's");
        assert_eq!(b.drafts(), vec![(1, "from b".to_owned())]);
        // The copy a made after the switch is cleaned up on a, by its UID.
        replay_times(&shared, &mut a, 1).await;
        assert!(a.drafts.is_empty(), "{:?}", a.drafts());
        assert_eq!(b.drafts().len(), 1, "and only on a");
    }

    #[tokio::test]
    async fn an_old_cleanup_cant_take_the_copy_made_after_moving_back() {
        let (shared, _rx) = drafts_engine();
        // a's upload can't confirm its copy's UID.
        let mut a = FakeServer { unsure: true, ..FakeServer::new(&shared, |_: &Shared| {}) };
        type_draft(&shared, 7, 1, "v1");
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        a.unsure = false;
        // Moved to b: a's copy goes, found by Message-ID; the first try fails and backs off.
        let to_b = Draft { account: "b".into(), ..draft("v2") };
        shared.with_writer(|st| drafts::save(st, 7, &to_b, true, at(2), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
        a.fail = true;
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        a.fail = false;
        // Back to a, with new text, uploaded before the cleanup's retry comes due.
        type_draft(&shared, 7, 3, "v3");
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        assert_eq!(a.drafts().iter().filter(|(_, s)| s == "v3").count(), 1, "{:?}", a.drafts());
        // The cleanup's retry finds v3 by its Message-ID too.
        replay_times(&shared, &mut a, 2).await;
        assert_eq!(a.drafts().into_iter().map(|(_, s)| s).collect::<Vec<_>>(), vec!["v3".to_owned()], "a ends up with the draft");
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty(), "and nothing left to do on a");
    }

    #[tokio::test]
    async fn an_old_cleanup_whose_answer_is_lost_still_gets_the_copy_put_back() {
        let (shared, _rx) = drafts_engine();
        let mut a = FakeServer { unsure: true, ..FakeServer::new(&shared, |_: &Shared| {}) };
        type_draft(&shared, 7, 1, "v1");
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        a.unsure = false;
        // Moved to b, a's cleanup failing for now; back to a, with new text uploaded.
        let to_b = Draft { account: "b".into(), ..draft("v2") };
        shared.with_writer(|st| drafts::save(st, 7, &to_b, true, at(2), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
        a.fail = true;
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        a.fail = false;
        type_draft(&shared, 7, 3, "v3");
        replay_with(&shared, &account("a"), &mut a).await.unwrap();
        // The cleanup's retry takes v3 too, and its answer is lost; the next finds nothing.
        a.cut_off = true;
        shared.with_writer(|st| {
            st.ops_due_now("a");
            Ok(())
        })
        .unwrap();
        assert!(replay_with(&shared, &account("a"), &mut a).await.is_err(), "the connection dropped");
        assert!(a.drafts.is_empty(), "{:?}", a.drafts());
        replay_times(&shared, &mut a, 3).await;
        assert_eq!(a.drafts().into_iter().map(|(_, s)| s).collect::<Vec<_>>(), vec!["v3".to_owned()], "a ends up with the draft");
        assert!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().is_empty());
    }

    /// The split inbox through the engine: a tab's list comes with every tab's unread count
    /// (before the whole list); a correction moves a conversation and its undo moves it back; a rule in the
    /// settings moves its sender's mail at once. None of it reaches a server (no worker runs).
    #[tokio::test]
    async fn the_split_inbox_lists_tabs_takes_corrections_and_follows_rules() {
        let dir = std::env::temp_dir().join(format!("envoy-engine-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        let st = Store::open(&db).unwrap();
        st.upsert_mailbox("a", "INBOX", "Inbox", Role::Inbox, "/", true).unwrap();
        st.set_classifier(crate::category::Classifier::new(vec![], ["a@x.com".to_owned()])).unwrap();
        let put = |uid: u32, headers: &str| {
            let p = crate::mime::parse(format!("{headers}\r\n\r\nbody\r\n").as_bytes(), true).unwrap();
            st.upsert_header("a", "INBOX", uid, 0, 10, uid as i64, &p).unwrap()
        };
        let news = put(1, "From: News <hello@news.example>\r\nTo: a@x.com\r\nSubject: Weekly\r\nMessage-ID: <n@x>\r\nList-Unsubscribe: <https://news.example/u>");
        let direct = put(2, "From: Sam <sam@x.org>\r\nTo: a@x.com\r\nSubject: Dinner?\r\nMessage-ID: <s@x>");
        let (shared, rx) = engine_with_reader(st, Store::open(&db).unwrap());
        shared.accounts.write().unwrap().push(account("a"));
        update_classifier(&shared);
        assert!(!shared.recategorizing.load(std::sync::atomic::Ordering::SeqCst), "the same classifier: nothing to do");
        let view = View::Role { account: None, role: Role::Inbox };
        let list = |req: u64, query: &str, category: Option<Category>| {
            let cmd = Command::List { req, view: view.clone(), query: query.into(), conversations: true, limit: 100, stream: false, category, counts: true };
            (shared.clone(), cmd)
        };
        let (s, cmd) = list(1, "", Some(Category::Newsletters));
        handle(&s, &mut HashMap::new(), cmd).await.unwrap();
        let mut events = Vec::new();
        while !events.iter().any(|e| matches!(e, Event::List { .. })) {
            events.push(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        }
        let counts = events.iter().position(|e| matches!(e, Event::CategoryCounts { .. })).expect("counts");
        let rows = events.iter().position(|e| matches!(e, Event::List { .. })).unwrap();
        assert!(counts < rows, "counts first: {events:?}");
        let Event::CategoryCounts { counts, .. } = &events[counts] else { unreachable!() };
        assert_eq!((counts[Category::Newsletters.code() as usize], counts[Category::Important.code() as usize]), (1, 1));
        let Event::List { items, .. } = &events[rows] else { unreachable!() };
        assert_eq!(items.iter().map(|m| (m.id, m.category)).collect::<Vec<_>>(), [(news, Category::Newsletters)]);
        // A search under the tabs: every category's rows, and the counts with them, under its req.
        let (s, cmd) = list(3, "from:sam", None);
        handle(&s, &mut HashMap::new(), cmd).await.unwrap();
        let mut events = Vec::new();
        while !events.iter().any(|e| matches!(e, Event::List { req: 3, .. })) {
            events.push(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        }
        assert!(events.iter().any(|e| matches!(e, Event::CategoryCounts { req: 3, .. })), "counts during a search: {events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::List { req: 3, items, .. } if items.len() == 1 && items[0].id == direct)), "{events:?}");
        let category = |id| shared.with_writer(|st| Ok(st.summary(id)?.unwrap().category)).unwrap();
        // Opened: why it's in its tab follows.
        handle(&shared, &mut HashMap::new(), Command::Open { req: 2, id: news, conversations: true }).await.unwrap();
        let why = loop {
            if let Event::Explained { id, category, why } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                break (id, category, why);
            }
        };
        assert_eq!(why, (news, Category::Newsletters, "Has an unsubscribe link".to_owned()));
        // A correction, and its undo.
        handle(&shared, &mut HashMap::new(), Command::MoveToCategory { ids: vec![news], category: Category::Important, undo: 9 }).await.unwrap();
        assert_eq!(category(news), Category::Important);
        assert!(rx.try_iter().any(|e| matches!(e, Event::Changed { .. })));
        handle(&shared, &mut HashMap::new(), Command::UndoCategory { undo: 9 }).await.unwrap();
        assert_eq!(category(news), Category::Newsletters);
        handle(&shared, &mut HashMap::new(), Command::UndoCategory { undo: 9 }).await.unwrap(); // twice: nothing
        // A rule for one sender, from settings: its mail moves now, no pass.
        let mut settings = Settings::default();
        settings.set("inbox.rules", serde_json::json!([{ "from": "sam@x.org", "category": "notifications" }])).unwrap();
        let _ = rx.try_iter().count();
        handle(&shared, &mut HashMap::new(), Command::Settings(Box::new(settings))).await.unwrap();
        assert_eq!(category(direct), Category::Notifications);
        assert!(rx.try_iter().any(|e| matches!(e, Event::Changed { .. })), "the list follows");
        assert!(!shared.recategorizing.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_save_the_store_refuses_is_never_reported_saved() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "hello");
        shared.with_writer(|st| st.discard_draft(7, None)).unwrap(); // gone for good
        // A too-late Undo put it back in the composer, which saves it, stamped by a clock
        // behind the database's.
        handle(&shared, &mut HashMap::new(), Command::SaveDraft { id: 7, draft: draft("hello again"), open: true, rev: 2, lifecycle: 0 }).await.unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(!events.iter().any(|e| matches!(e, Event::DraftSaved { id: 7, rev: 2 })), "not saved, so not acked: {events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::DraftSaveRefused { id: 7, rev: 2 })), "told it's refused: {events:?}");
    }

    #[tokio::test]
    async fn a_save_copes_with_the_old_copy_already_gone() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "first");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        server.drafts.clear(); // deleted from another computer
        type_draft(&shared, 7, 2, "second");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(2, "second".to_owned())]);
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 0);
    }

    #[tokio::test]
    async fn offline_saves_wait_in_the_queue() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "written offline");
        let mut server = FakeServer { fail: true, ..FakeServer::new(&shared, |_: &Shared| {}) };
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.drafts.is_empty());
        shared
            .with_writer(|st| {
                let q = st.ops("a", i64::MAX)?;
                assert_eq!((q.len(), q[0].attempts), (1, 1), "kept for a retry");
                assert_eq!(st.draft(7)?.unwrap().draft.subject, "written offline", "safe here meanwhile");
                st.queue_draft_upload("a", 7, 0) // the retry comes due
            })
            .unwrap();
        server.fail = false;
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(1, "written offline".to_owned())]);
    }

    fn errors(rx: &std::sync::mpsc::Receiver<Event>) -> Vec<String> {
        rx.try_iter().filter_map(|e| if let Event::Error(t) = e { Some(t) } else { None }).collect()
    }

    /// Replays `n` times, the retry's wait passing in between.
    async fn replay_times(shared: &Arc<Shared>, server: &mut FakeServer, n: usize) {
        for _ in 0..n {
            shared.with_writer(|st| {
                st.ops_due_now("a");
                Ok(())
            })
            .unwrap();
            replay_with(shared, &account("a"), server).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_busy_server_never_makes_a_draft_upload_give_up() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "hello");
        let mut server = FakeServer { fail: true, ..FakeServer::new(&shared, |_: &Shared| {}) }; // NO [UNAVAILABLE]
        replay_times(&shared, &mut server, 8).await;
        shared
            .with_writer(|st| {
                let q = st.ops("a", i64::MAX)?;
                assert_eq!((q.len(), q[0].attempts), (1, 8), "still queued after eight tries");
                assert!(st.draft(7)?.unwrap().visible);
                Ok(())
            })
            .unwrap();
        assert!(errors(&rx).is_empty(), "trouble that clears by itself isn't reported");
        server.fail = false;
        replay_times(&shared, &mut server, 1).await;
        assert_eq!(server.drafts(), vec![(1, "hello".to_owned())]);
    }

    #[tokio::test]
    async fn edits_during_failing_uploads_dont_pile_up_retries() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "v1");
        for rev in 2..8 {
            shared
                .with_writer(|st| {
                    st.ops_due_now("a");
                    Ok(())
                })
                .unwrap();
            // Edited while each try is on the wire, and each try fails.
            let mut server = FakeServer { fail: true, ..FakeServer::new(&shared, move |s: &Shared| type_draft(s, 7, rev, &format!("v{rev}"))) };
            replay_with(&shared, &account("a"), &mut server).await.unwrap();
        }
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 1, "one upload waiting");
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_times(&shared, &mut server, 1).await;
        assert_eq!(server.drafts(), vec![(1, "v7".to_owned())], "with every edit");
    }

    #[tokio::test]
    async fn a_refused_draft_upload_is_reported_and_kept() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "hello");
        let mut server = FakeServer { fail: true, failure: "code: Some(TryCreate), info: Some(\"no such mailbox\")", ..FakeServer::new(&shared, |_: &Shared| {}) };
        replay_times(&shared, &mut server, 1).await;
        let told = errors(&rx);
        assert!(told.len() == 1 && told[0].contains("safe on this computer"), "{told:?}");
        replay_times(&shared, &mut server, 4).await;
        assert!(errors(&rx).is_empty(), "not on every retry");
        replay_times(&shared, &mut server, 1).await;
        assert_eq!(errors(&rx).len(), 1, "again every few");
        assert_eq!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().len(), 1, "and kept: it'll go once the folder exists");
    }

    #[tokio::test]
    async fn a_sent_drafts_cleanup_failing_never_brings_it_back() {
        let (shared, rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "hello");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        handle(&shared, &mut HashMap::new(), Command::Send { draft: draft("hello"), draft_id: Some(7) }).await.unwrap();
        replay_with(&shared, &account("a"), &mut server).await.unwrap(); // sent; the purge is queued
        server.fail = true;
        server.failure = "code: Some(Parse), info: Some(\"bad\")";
        for _ in 0..8 {
            replay_times(&shared, &mut server, 1).await;
            assert!(drafts_list(&shared, "").is_empty(), "a sent draft stays gone");
            assert!(!shared.with_writer(|st| st.draft(7)).unwrap().unwrap().visible);
        }
        assert!(errors(&rx).iter().all(|e| !e.starts_with("Gave up")));
        server.fail = false;
        replay_times(&shared, &mut server, 1).await;
        assert!(server.drafts.is_empty() && shared.with_writer(|st| st.draft(7)).unwrap().is_none(), "cleaned up once the server lets it");
    }

    #[tokio::test]
    async fn an_edit_during_the_upload_goes_up_next() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "v1");
        let mut server = FakeServer::new(&shared, |s: &Shared| type_draft(s, 7, 2, "v2"));
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(1, "v1".to_owned())]);
        assert!(shared.with_writer(|st| st.draft_needs_upload(7)).unwrap(), "v2 still to go");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(2, "v2".to_owned())], "one copy, the newest");
    }

    #[tokio::test]
    async fn discarding_during_the_upload_leaves_nothing_behind() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "v1");
        let mut server = FakeServer::new(&shared, |s: &Shared| {
            s.with_writer(|st| st.discard_draft(7, Some((now() - 1, 3)))).unwrap();
        });
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts.len(), 1, "the upload landed");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.drafts.is_empty(), "then the discard took it");
        assert_eq!(shared.with_writer(|st| st.draft(7)).unwrap(), None);
    }

    #[tokio::test]
    async fn sending_removes_the_draft_here_and_on_the_server() {
        let (shared, _rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "hello");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let copy = sync_copy(&shared, &server, 1);
        assert_eq!(drafts_list(&shared, "").iter().map(|s| s.id).collect::<Vec<_>>(), vec![copy]);
        handle(&shared, &mut HashMap::new(), Command::Send { draft: draft("hello"), draft_id: Some(7) }).await.unwrap();
        assert!(drafts_list(&shared, "").is_empty(), "off the Drafts list at once");
        replay_with(&shared, &account("a"), &mut server).await.unwrap(); // sends
        assert!(matches!(server.ran.last(), Some(Op::Send { .. })));
        replay_with(&shared, &account("a"), &mut server).await.unwrap(); // then purges
        assert!(server.drafts.is_empty(), "off the server");
        shared
            .with_writer(|st| {
                assert_eq!(st.draft(7)?, None, "and gone here");
                assert!(st.summary(copy)?.is_none(), "cached copy too");
                assert_eq!(st.pending_count()?, 0);
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn the_message_replied_to_is_marked_answered_on_its_own_account() {
        let (shared, _rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        // The message is in a's INBOX at UID 1; b's INBOX has another message at UID 1.
        let original = shared
            .with_writer(|st| {
                st.set_uidvalidity("a", "INBOX", 7)?;
                st.set_uidvalidity("b", "INBOX", 3)?;
                message_in(st, "b", "INBOX", 1, "<bystander@x>", "", 0);
                Ok(message_in(st, "a", "INBOX", 1, "<original@x>", "", 0))
            })
            .unwrap();
        // Replied to from b.
        let reply = Draft { account: "b".into(), reply_of: Some(original), ..draft("Re: Plans") };
        handle(&shared, &mut HashMap::new(), Command::Send { draft: reply, draft_id: None }).await.unwrap();
        let mut b = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("b"), &mut b).await.unwrap();
        assert!(matches!(b.ran.as_slice(), [Op::Send { .. }]), "sent, and nothing marked on b: {:?}", b.ran);
        assert!(shared.with_writer(|st| st.ops("b", i64::MAX)).unwrap().is_empty());
        // a marks it, by the UID, generation and Message-ID it had when the reply was sent.
        let on_a: Vec<(Op, At)> = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().map(|q| (q.op, q.at)).collect();
        assert_eq!(
            on_a,
            [(Op::SetFlags { mailbox: "INBOX".into(), uid: 1, add: flags::ANSWERED, remove: 0 }, At { validity: 7, message_id: "<original@x>".into() })]
        );
        // A message whose mailbox's generation isn't known can't be pinned down: a reply to it
        // marks nothing, anywhere.
        let unknown = shared.with_writer(|st| Ok(message_in(st, "a", "Archive", 4, "<unknown@x>", "", 0))).unwrap();
        let reply = Draft { account: "b".into(), reply_of: Some(unknown), ..draft("Re: Old plans") };
        handle(&shared, &mut HashMap::new(), Command::Send { draft: reply, draft_id: None }).await.unwrap();
        replay_with(&shared, &account("b"), &mut b).await.unwrap();
        assert_eq!(b.ran.iter().filter(|op| matches!(op, Op::Send { .. })).count(), 2);
        assert_eq!(shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().len(), 1, "only the first reply's");
        assert!(shared.with_writer(|st| st.ops("b", i64::MAX)).unwrap().is_empty());
    }

    #[tokio::test]
    async fn undo_send_brings_the_draft_back_without_a_copy() {
        let (shared, rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "hello");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        handle(&shared, &mut HashMap::new(), Command::Send { draft: draft("hello, edited"), draft_id: Some(7) }).await.unwrap();
        let op = rx.try_iter().find_map(|e| if let Event::Sent { op, .. } = e { Some(op) } else { None }).expect("queued");
        handle(&shared, &mut HashMap::new(), Command::UndoSend { op }).await.unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        let opened = events.iter().find_map(|e| if let Event::DraftOpened { id, draft, restored, .. } = e { Some((*id, draft.subject.clone(), *restored)) } else { None });
        assert_eq!(opened, Some((7, "hello, edited".to_owned(), false)), "the composer gets it back as it was sent");
        let list = drafts_list(&shared, "");
        assert_eq!(list.len(), 1, "one draft, not two");
        assert_eq!(list[0].subject, "hello, edited");
        // The edit made just before sending reaches the server, replacing the old copy.
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(2, "hello, edited".to_owned())]);
    }

    /// `draft` as the composer shows draft `id`: with the files the store has.
    fn as_shown(shared: &Shared, id: i64, draft: Draft) -> Draft {
        Draft { attachments: shared.with_writer(|st| st.files_of(id)).unwrap(), ..draft }
    }

    /// Sends draft `id` as `draft`, which must be refused: why, as the composer is told.
    async fn refused(shared: &Arc<Shared>, rx: &std::sync::mpsc::Receiver<Event>, id: i64, draft: Draft) -> String {
        handle(shared, &mut HashMap::new(), Command::Send { draft, draft_id: Some(id) }).await.unwrap();
        rx.try_iter().find_map(|e| if let Event::SendFailed { draft_id: Some(i), error } = e { (i == id).then_some(error) } else { None }).expect("refused, the composer told")
    }

    #[tokio::test]
    async fn a_send_that_cant_be_queued_goes_back_to_the_composer() {
        let (shared, rx) = drafts_engine();
        let unsendable = Draft { account: "removed".into(), ..draft("unsendable") };
        assert_eq!(refused(&shared, &rx, 9, unsendable).await, "Choose an account to send from", "the composer gets it back, saying why");
        let kept = shared.with_writer(|st| st.draft(9)).unwrap().unwrap();
        assert!(kept.visible && kept.draft.subject == "unsendable", "and it's kept as a draft meanwhile");
    }

    #[tokio::test]
    async fn a_draft_save_is_confirmed_only_once_written() {
        let (shared, rx) = drafts_engine();
        handle(&shared, &mut HashMap::new(), Command::SaveDraft { id: 7, draft: draft("hello"), open: true, rev: 5, lifecycle: 0 }).await.unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::DraftSaved { id: 7, rev: 5 })));
        // The disk fills up: the app hears it wasn't saved, never that it was.
        shared
            .with_writer(|st| {
                st.break_drafts();
                Ok(())
            })
            .unwrap();
        handle(&shared, &mut HashMap::new(), Command::SaveDraft { id: 7, draft: draft("hello again"), open: true, rev: 6, lifecycle: 0 }).await.unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::DraftSaveFailed { id: 7, rev: 6, .. })), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, Event::DraftSaved { rev: 6, .. })));
    }

    #[tokio::test]
    async fn deleting_a_local_drafts_row_discards_it() {
        let (shared, rx) = drafts_engine();
        // Not uploaded yet: its row is the draft itself (a negative id).
        shared.with_writer(|st| drafts::save(st, 7, &draft("only here"), true, at(1), now())).unwrap();
        let row = drafts::list_id(7);
        assert_eq!(drafts_list(&shared, "").iter().map(|r| r.id).collect::<Vec<_>>(), vec![row]);
        // What Delete used to send: a message move, which can't touch a draft.
        handle(&shared, &mut HashMap::new(), Command::Move { ids: vec![row], to: MoveTarget::Trash, thread: false, undo: None }).await.unwrap();
        assert_eq!(drafts_list(&shared, "").len(), 1);
        handle(&shared, &mut HashMap::new(), Command::DiscardDraftRow { row, undo: Some(3) }).await.unwrap();
        assert!(drafts_list(&shared, "").is_empty(), "discarded");
        assert!(rx.try_iter().any(|e| matches!(e, Event::DraftDiscarded { id: 7, undo: Some(3) })), "a composer holding it is told");
        undo_move(&shared, 3).unwrap();
        assert_eq!(drafts_list(&shared, "").len(), 1, "Undo brings it back");
        handle(&shared, &mut HashMap::new(), Command::DiscardDraftRow { row, undo: None }).await.unwrap();
        assert!(shared.with_writer(|st| st.drafts(None)).unwrap().is_empty(), "never on the server: gone at once");
    }

    #[tokio::test]
    async fn deleting_a_drafts_server_copy_discards_the_draft_for_good() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "uploaded");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let copy = sync_copy(&shared, &server, 1);
        handle(&shared, &mut HashMap::new(), Command::DiscardDraftRow { row: copy, undo: None }).await.unwrap();
        assert!(drafts_list(&shared, "").is_empty());
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.drafts.is_empty(), "off the server");
        assert!(shared.with_writer(|st| st.draft(7)).unwrap().is_none(), "the local draft too");
        assert!(drafts_list(&shared, "").is_empty(), "and it doesn't come back");
    }

    #[tokio::test]
    async fn deleting_a_server_only_drafts_row_trashes_it_undoably() {
        let (shared, _rx) = drafts_engine();
        let id = server_copy(&shared, 5, &draft_with_pdf(false));
        handle(&shared, &mut HashMap::new(), Command::DiscardDraftRow { row: id, undo: Some(4) }).await.unwrap();
        assert!(drafts_list(&shared, "").is_empty());
        let held: Vec<Op> = shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().map(|q| q.op).collect();
        assert_eq!(held, vec![Op::Move { mailbox: "Drafts".into(), uid: 5, dest: "Trash".into() }]);
        undo_move(&shared, 4).unwrap();
        assert_eq!(drafts_list(&shared, "").iter().map(|r| r.id).collect::<Vec<_>>(), vec![id]);
    }

    #[tokio::test]
    async fn undoing_a_discard_keeps_the_draft() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "hello");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        handle(&shared, &mut HashMap::new(), Command::DiscardDraft { id: 7, undo: Some(11) }).await.unwrap();
        assert!(drafts_list(&shared, "").is_empty());
        undo_move(&shared, 11).unwrap();
        assert_eq!(drafts_list(&shared, "").len(), 1, "back");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts.len(), 1, "the server never lost it");
    }

    #[test]
    fn the_drafts_list_shows_local_drafts_newest_content() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "only here");
        let rows = drafts_list(&shared, "");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.id, row.subject.as_str(), row.preview.as_str()), (drafts::list_id(7), "only here", "About only here."));
        assert_eq!(row.to, vec![crate::model::Address { name: "Bo".into(), email: "bo@x.com".into() }]);
        assert_eq!((row.from.email.as_str(), row.mailbox.as_str()), ("a@x.com", "Drafts"));
        assert!(row.flags & flags::DRAFT != 0 && row.seen());
        assert_eq!(drafts_list(&shared, "only").len(), 1, "search finds it");
        assert!(drafts_list(&shared, "elsewhere").is_empty());
        assert!(drafts_list(&shared, "is:unread").is_empty());
        // Not on other lists.
        let inbox = View::Role { account: None, role: Role::Inbox };
        let q = Query::default();
        assert!(shared.with_writer(|st| drafts::merge(st, &inbox, &q, 10, vec![], |a| shared.sender(a))).unwrap().is_empty());
    }

    #[test]
    fn a_copy_flagged_deleted_doesnt_show() {
        // Without UIDPLUS, replaced and discarded copies are only flagged \Deleted on the server,
        // and a sync can bring one into the cache.
        let (shared, _rx) = drafts_engine();
        let raw = b"From: a@x.com\r\nSubject: Old version\r\nMessage-ID: <old@x>\r\n\r\nhi\r\n";
        let p = crate::mime::parse(raw, true).unwrap();
        shared.with_writer(|st| st.upsert_header("a", "Drafts", 5, flags::DRAFT | flags::DELETED, 10, 0, &p)).unwrap();
        assert!(drafts_list(&shared, "").is_empty());
    }

    #[tokio::test]
    async fn a_cached_copy_shows_the_local_draft() {
        let (shared, _rx) = drafts_engine();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        type_draft(&shared, 7, 1, "uploaded");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let copy = sync_copy(&shared, &server, 1);
        type_draft(&shared, 7, 2, "edited since"); // the server is behind
        let rows = drafts_list(&shared, "");
        assert_eq!(rows.iter().map(|r| (r.id, r.subject.as_str())).collect::<Vec<_>>(), vec![(copy, "edited since")], "one row, the newest content");
        // Opening the copy opens the local draft, not the stale server version.
        match open_draft(&shared, copy, now()).unwrap() {
            Opening::Ready(d, _) => assert_eq!((d.id, d.draft.subject.as_str()), (7, "edited since")),
            _ => panic!("should open"),
        }
    }

    #[tokio::test]
    async fn a_draft_only_the_server_has_becomes_editable() {
        let (shared, _rx) = drafts_engine();
        // Written on another computer: in Drafts, not downloaded yet.
        let raw = b"From: a@x.com\r\nTo: \"Lima, Ana\" <ana@x.com>\r\nBcc: bo@x.com\r\nSubject: From the laptop\r\nMessage-ID: <laptop@x>\r\nIn-Reply-To: <orig@x>\r\nReferences: <root@x> <orig@x>\r\n\r\nHalf written.\r\n";
        let p = crate::mime::parse(raw, true).unwrap();
        let id = shared.with_writer(|st| st.upsert_header("a", "Drafts", 5, flags::DRAFT, 10, 0, &p)).unwrap();
        assert!(matches!(open_draft(&shared, id, now()).unwrap(), Opening::Download { uid: 5, .. }));
        shared.with_writer(|st| st.set_body(id, raw, &crate::mime::parse(raw, false).unwrap())).unwrap();
        let Opening::Ready(d, notice) = open_draft(&shared, id, now()).unwrap() else { panic!("should open") };
        assert_eq!(notice, None, "plain text: nothing lost editing it");
        assert_eq!(d.message_id, "<laptop@x>", "keeps its Message-ID, so a save replaces it");
        assert_eq!(d.draft.to, "\"Lima, Ana\" <ana@x.com>");
        assert_eq!(d.draft.bcc, "bo@x.com");
        assert_eq!((d.draft.in_reply_to.as_str(), d.draft.references.as_str()), ("<orig@x>", "<root@x> <orig@x>"));
        assert_eq!(d.draft.body, "Half written.\n");
        assert_eq!(drafts_list(&shared, "").len(), 1, "one row for it");
        // Edited here: the save replaces the laptop's copy.
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.drafts.push((5, "<laptop@x>".into(), raw.to_vec()));
        server.next_uid = 6;
        let edited = Draft { body: "Finished.".into(), ..d.draft.clone() };
        shared.with_writer(|st| drafts::save(st, d.id, &edited, true, at(1), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts(), vec![(6, "From the laptop".to_owned())]);
        assert!(shared.with_writer(|st| st.summary(id)).unwrap().is_none(), "the laptop's copy is gone from the cache too");
    }

    const PDF: &[u8] = b"%PDF-1.4 not really, but binary enough \x00\x01\x02\xff";

    /// A draft written elsewhere: text, an HTML version if `html`, and a PDF.
    fn draft_with_pdf(html: bool) -> Vec<u8> {
        use base64::Engine as _;
        let pdf = base64::engine::general_purpose::STANDARD.encode(PDF);
        let text = if html {
            "Content-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n--alt\r\nContent-Type: text/plain\r\n\r\nPlan attached.\r\n--alt\r\nContent-Type: text/html\r\n\r\n<p>Plan <b>attached</b>.</p>\r\n--alt--\r\n"
        } else {
            "Content-Type: text/plain\r\n\r\nPlan attached.\r\n"
        };
        format!(
            "From: a@x.com\r\nTo: bo@x.com\r\nSubject: The plan\r\nMessage-ID: <laptop@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n--mix\r\n{text}--mix\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"plan.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{pdf}\r\n--mix--\r\n"
        )
        .into_bytes()
    }

    /// Caches a draft from the server, downloaded, as Drafts message `uid`.
    fn server_copy(shared: &Shared, uid: u32, raw: &[u8]) -> MessageId {
        let p = crate::mime::parse(raw, false).unwrap();
        shared
            .with_writer(|st| {
                let id = st.upsert_header("a", "Drafts", uid, flags::DRAFT | flags::SEEN, raw.len() as u32, 0, &p)?;
                st.set_body(id, raw, &p)?;
                Ok(id)
            })
            .unwrap()
    }

    fn files_of(raw: &[u8]) -> Vec<Vec<u8>> {
        crate::mime::server_draft(raw).unwrap().files
    }

    #[tokio::test]
    async fn editing_and_sending_a_server_draft_keeps_its_attachments() {
        let (shared, _rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        let raw = draft_with_pdf(false);
        let id = server_copy(&shared, 5, &raw);
        let Opening::Ready(d, notice) = open_draft(&shared, id, now()).unwrap() else { panic!("should open") };
        assert_eq!(notice, None);
        assert_eq!(d.draft.attachments.iter().map(|a| (a.filename.as_str(), a.size)).collect::<Vec<_>>(), vec![("plan.pdf", PDF.len())], "the composer shows it");
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.drafts.push((5, "<laptop@x>".into(), raw.clone()));
        server.next_uid = 6;
        // Edited: the autosave's upload replaces the original, attachment and all.
        let edited = Draft { body: "Plan attached, see page 2.".into(), ..d.draft.clone() };
        shared.with_writer(|st| drafts::save(st, d.id, &edited, true, at(1), now() - drafts::UPLOAD_DELAY - 1)).unwrap();
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.drafts.len(), 1);
        let (_, _, copy) = &server.drafts[0];
        assert_eq!(files_of(copy), vec![PDF.to_vec()], "the attachment survives the edit");
        assert!(crate::mime::server_draft(copy).unwrap().draft.body.contains("page 2"));
        // Sent: with the attachment.
        handle(&shared, &mut HashMap::new(), Command::Send { draft: edited, draft_id: Some(d.id) }).await.unwrap();
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let Some(Op::Send { raw_b64, .. }) = server.ran.iter().find(|op| matches!(op, Op::Send { .. })) else { panic!("not sent") };
        let sent = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.decode(raw_b64).unwrap()
        };
        assert_eq!(files_of(&sent), vec![PDF.to_vec()], "and goes with the message");
    }

    #[test]
    fn a_formatted_server_draft_keeps_its_attachments_and_says_what_goes() {
        let (shared, _rx) = drafts_engine();
        let id = server_copy(&shared, 5, &draft_with_pdf(true));
        let Opening::Ready(d, notice) = open_draft(&shared, id, now()).unwrap() else { panic!("should open") };
        assert_eq!(notice, Some(DraftNotice::PlainText));
        assert_eq!(d.draft.body.trim_end(), "Plan attached.");
        assert_eq!(shared.with_writer(|st| st.attached_files(d.id)).unwrap()[0].1, PDF.to_vec());
    }

    #[tokio::test]
    async fn a_draft_envoy_cant_edit_in_place_opens_as_a_copy_and_stays() {
        let (shared, _rx) = drafts_engine();
        let raw = b"From: a@x.com\r\nTo: bo@x.com\r\nSubject: Signed\r\nMessage-ID: <signed@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"s\"\r\n\r\n--s\r\nContent-Type: text/plain\r\n\r\nSigned words.\r\n--s\r\nContent-Type: application/pgp-signature\r\n\r\n-----BEGIN PGP SIGNATURE-----\r\nxyz\r\n-----END PGP SIGNATURE-----\r\n--s--\r\n";
        let id = server_copy(&shared, 5, raw);
        let Opening::Copy(copy, why) = open_draft(&shared, id, now()).unwrap() else { panic!("a copy") };
        assert!(why.contains("signed"), "{why}");
        assert_eq!((copy.account.as_str(), copy.subject.as_str()), ("a", "Signed"));
        assert!(copy.body.contains("Signed words."));
        assert!(shared.with_writer(|st| st.drafts(None)).unwrap().is_empty(), "nothing adopted: the original isn't Envoy's to replace");
        assert_eq!(drafts_list(&shared, "").iter().map(|r| r.id).collect::<Vec<_>>(), vec![id], "it stays in Drafts");
    }

    // ── attachments ──

    /// Events up to and including the first `want` matches of `done`, waiting for work the
    /// engine does off its loop.
    fn wait_for(rx: &std::sync::mpsc::Receiver<Event>, want: usize, done: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let mut found = 0;
        while found < want {
            let e = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("waited for {want}, got {found}: {seen:?}"));
            found += done(&e) as usize;
            seen.push(e);
        }
        seen
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suggestions_come_back_with_their_request_and_leave_out_my_addresses() {
        let st = setup();
        st.upsert_mailbox("a", "Sent", "Sent", Role::Sent, "/", true).unwrap();
        for (uid, (mailbox, from, to)) in [("Sent", "Me <a@x.com>", "Ana Lima <ana@x.com>"), ("INBOX", "Andy <andy@y.org>", "a@x.com"), ("INBOX", "a@x.com", "a@x.com")].into_iter().enumerate() {
            let raw = format!("From: {from}\r\nTo: {to}\r\nSubject: hi\r\nMessage-ID: <{uid}@x>\r\nDate: {}\r\n\r\nhi\r\n", chrono::Utc::now().to_rfc2822());
            st.upsert_header("a", mailbox, uid as u32 + 1, flags::SEEN, 10, 0, &crate::mime::parse(raw.as_bytes(), true).unwrap()).unwrap();
        }
        let (shared, rx) = engine_with(Store::memory());
        *shared.reader.lock().unwrap() = st;
        shared.accounts.write().unwrap().push(account("a"));
        handle(&shared, &mut HashMap::new(), Command::Contacts { req: 7, typed: "a".into() }).await.unwrap();
        let events = wait_for(&rx, 1, |e| matches!(e, Event::Contacts { .. }));
        let Some(Event::Contacts { req, contacts }) = events.last() else { unreachable!() };
        assert_eq!(*req, 7);
        assert_eq!(contacts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), vec!["ana@x.com", "andy@y.org"], "the one I wrote to first; never me");
        assert_eq!(contacts[0].name, "Ana Lima");
    }

    #[test]
    fn the_store_learns_my_addresses_as_accounts_come_and_go() {
        let (shared, _rx) = engine_with(setup());
        shared.accounts.write().unwrap().push(account("a"));
        shared.store_me();
        let sent = "From: a@x.com\r\nTo: Bo <bo@y.org>\r\nSubject: hi\r\nMessage-ID: <1@x>\r\n\r\nhi\r\n";
        shared.with_writer(|st| st.upsert_header("a", "Archive", 1, 0, 10, 0, &crate::mime::parse(sent.as_bytes(), true).unwrap())).unwrap();
        let suggest = |typed: &str| shared.with_writer(|st| st.suggest(typed, &HashSet::new(), now(), 5)).unwrap();
        assert_eq!(suggest("bo").len(), 1);
        assert!(suggest("a@").is_empty(), "mail from my address, outside Sent, is mine: its sender isn't a contact");
    }

    fn attached(events: &[Event]) -> Vec<crate::model::DraftAttachment> {
        events.iter().filter_map(|e| if let Event::FileAttached { file, .. } = e { Some(file.clone()) } else { None }).collect()
    }

    fn sent_raw(server: &FakeServer) -> Vec<u8> {
        use base64::Engine as _;
        let Some(Op::Send { raw_b64, .. }) = server.ran.iter().find(|op| matches!(op, Op::Send { .. })) else { panic!("not sent") };
        base64::engine::general_purpose::STANDARD.decode(raw_b64).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_files_are_kept_uploaded_and_sent_with_the_draft() {
        let (shared, rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        let dir = std::env::temp_dir().join(format!("envoy-engine-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (pdf, photo) = (dir.join("Plan for Café.pdf"), dir.join("photo.png"));
        std::fs::write(&pdf, PDF).unwrap();
        std::fs::write(&photo, b"\x89PNG\r\n\x1a\n").unwrap();
        // The composer saves first, then hands the files over.
        type_draft(&shared, 7, 1, "plan");
        handle(&shared, &mut HashMap::new(), Command::AttachFiles { draft: 7, files: vec![(0, pdf), (1, photo), (2, dir.join("gone.txt"))] }).await.unwrap();
        let events = wait_for(&rx, 3, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. }));
        let files = attached(&events);
        assert_eq!(files.iter().map(|f| (f.id, f.filename.as_str(), f.mime.as_str(), f.size)).collect::<Vec<_>>(), [(0, "Plan for Café.pdf", "application/pdf", PDF.len()), (1, "photo.png", "image/png", 8)]);
        assert!(events.iter().any(|e| matches!(e, Event::AttachFailed { draft: 7, file: 2, error } if error.starts_with("Couldn't read gone.txt"))), "{events:?}");
        // In the draft, with the store, once attached: its upload carries them.
        assert_eq!(uploaded_files(&shared).await.into_iter().map(|(_, d)| d).collect::<Vec<_>>(), vec![PDF.to_vec(), b"\x89PNG\r\n\x1a\n".to_vec()], "the Drafts copy");
        // Not sent while a file it shows isn't attached; taken off, it goes, with every file.
        let shown = as_shown(&shared, 7, draft("plan"));
        assert_eq!(refused(&shared, &rx, 7, shown).await, "gone.txt isn't attached: remove it, or attach it again");
        handle(&shared, &mut HashMap::new(), Command::DetachFile { draft: 7, file: 2 }).await.unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        handle(&shared, &mut HashMap::new(), Command::Send { draft: as_shown(&shared, 7, draft("plan")), draft_id: Some(7) }).await.unwrap();
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(files_of(&sent_raw(&server)), vec![PDF.to_vec(), b"\x89PNG\r\n\x1a\n".to_vec()], "and the message: every file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_for_a_draft_not_in_the_store_isnt_kept() {
        let (shared, rx) = drafts_engine();
        let path = std::env::temp_dir().join(format!("envoy-engine-orphan-{}.txt", std::process::id()));
        std::fs::write(&path, b"hi").unwrap();
        handle(&shared, &mut HashMap::new(), Command::AttachFiles { draft: 99, files: vec![(0, path.clone())] }).await.unwrap();
        let events = wait_for(&rx, 1, |e| matches!(e, Event::AttachFailed { .. } | Event::FileAttached { .. }));
        assert!(matches!(events.last(), Some(Event::AttachFailed { draft: 99, file: 0, error }) if error.contains("isn't saved")), "{events:?}");
        assert!(shared.with_writer(|st| st.files_of(99)).unwrap().is_empty(), "nothing kept for it");
        let _ = std::fs::remove_file(path);
    }

    /// A file named `name` holding `data`, in a folder of the test's own.
    fn file_at(test: &str, name: &str, data: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("envoy-engine-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    /// The files of the draft's copy in Drafts, uploaded now: name and bytes.
    async fn uploaded_files(shared: &Arc<Shared>) -> Vec<(String, Vec<u8>)> {
        let mut server = FakeServer::new(shared, |_: &Shared| {});
        shared.with_writer(|st| st.queue_draft_upload("a", 7, 0)).unwrap();
        replay_with(shared, &account("a"), &mut server).await.unwrap();
        let found = crate::mime::server_draft(&server.drafts.last().expect("uploaded").2).unwrap();
        found.draft.attachments.into_iter().map(|a| a.filename).zip(found.files).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_still_attaching_when_its_composer_goes_lands_in_the_draft() {
        let (shared, rx) = drafts_engine();
        let path = file_at("pending", "slow.pdf", PDF);
        type_draft(&shared, 7, 1, "plan");
        handle(&shared, &mut HashMap::new(), Command::AttachFiles { draft: 7, files: vec![(1, path)] }).await.unwrap();
        // The composer goes away before the file is in (another message started, or the app
        // closed): its last save lists no file, and nobody is there to hear the answer.
        type_draft(&shared, 7, 2, "plan, put away");
        wait_for(&rx, 1, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. }));
        let listed: Vec<String> = shared.with_writer(|st| st.draft(7)).unwrap().unwrap().draft.attachments.into_iter().map(|a| a.filename).collect();
        assert_eq!(listed, ["slow.pdf"], "kept with the draft, for whoever opens it next");
        assert_eq!(uploaded_files(&shared).await, [("slow.pdf".to_owned(), PDF.to_vec())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_attachment_id_is_never_taken_twice_nor_its_bytes_replaced() {
        let (shared, rx) = drafts_engine();
        let (a, b) = (file_at("ids", "a.pdf", b"AAAA"), file_at("ids", "b.png", b"BBBBBBBB"));
        type_draft(&shared, 7, 1, "x");
        handle(&shared, &mut HashMap::new(), Command::AttachFiles { draft: 7, files: vec![(5, a)] }).await.unwrap();
        let first = attached(&wait_for(&rx, 1, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. })));
        // A composer that lost count (reopened, say) gives another file the same id.
        handle(&shared, &mut HashMap::new(), Command::AttachFiles { draft: 7, files: vec![(5, b)] }).await.unwrap();
        let events = wait_for(&rx, 1, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. }));
        assert!(matches!(events.last(), Some(Event::AttachFailed { draft: 7, file: 5, .. })), "refused: {events:?}");
        // a.pdf keeps its bytes: what goes under its name is a.pdf.
        let listed = Draft { attachments: first, ..draft("x") };
        shared.with_writer(|st| drafts::save(st, 7, &listed, true, at(2), now() - 600)).unwrap();
        assert_eq!(uploaded_files(&shared).await, [("a.pdf".to_owned(), b"AAAA".to_vec())]);
    }

    /// A message with a PDF and a picture in the inbox, as `uid`: downloaded unless `headers_only`.
    fn mail_with_files(shared: &Shared, uid: u32, headers_only: bool) -> (MessageId, Vec<u8>) {
        use base64::Engine as _;
        let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
        let raw = format!(
            "From: Ana <ana@x.com>\r\nTo: a@x.com\r\nSubject: The plan\r\nMessage-ID: <plan@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n--mix\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--mix\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename*=utf-8''Pl%C3%A1n.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--mix\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=\"map.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--mix--\r\n",
            b64(PDF),
            b64(b"\x89PNG map")
        )
        .into_bytes();
        let p = crate::mime::parse(&raw, false).unwrap();
        let id = shared
            .with_writer(|st| {
                let id = st.upsert_header("a", "INBOX", uid, flags::SEEN, raw.len() as u32, 0, &p)?;
                if !headers_only {
                    st.set_body(id, &raw, &p)?;
                }
                Ok(id)
            })
            .unwrap();
        (id, raw)
    }

    /// A file of a forward, as the composer lists it from the original.
    fn forwarded(id: i64, name: &str) -> crate::model::DraftAttachment {
        crate::model::DraftAttachment { id, filename: name.into(), mime: crate::attach::type_for(name).into(), ..Default::default() }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forwarding_takes_the_originals_files_along() {
        let (shared, rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        let (original, raw) = mail_with_files(&shared, 3, false);
        let listed = shared.with_writer(|st| st.body(original)).unwrap().unwrap().attachments;
        assert_eq!(listed.iter().map(|a| (a.index, a.filename.as_str())).collect::<Vec<_>>(), [(0, "Plán.pdf"), (1, "map.png")]);
        type_draft(&shared, 7, 1, "Fwd: The plan");
        // The forward keeps the picture only (the PDF was taken off), as file 4.
        handle(&shared, &mut HashMap::new(), Command::AttachForwarded { draft: 7, message: original, files: vec![(forwarded(4, "map.png"), 1)] }).await.unwrap();
        let files = attached(&wait_for(&rx, 1, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. })));
        assert_eq!(files, vec![crate::model::DraftAttachment { id: 4, filename: "map.png".into(), mime: "image/png".into(), size: 8, ..Default::default() }]);
        handle(&shared, &mut HashMap::new(), Command::Send { draft: as_shown(&shared, 7, draft("Fwd: The plan")), draft_id: Some(7) }).await.unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(files_of(&sent_raw(&server)), vec![b"\x89PNG map".to_vec()]);
        assert_eq!(files_of(&raw)[1], b"\x89PNG map".to_vec(), "the original's bytes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_forward_takes_the_pictures_shown_inside_the_original_along() {
        use base64::Engine as _;
        let (shared, rx) = drafts_engine();
        let logo = b"\x89PNG logo".to_vec();
        let raw = format!(
            "From: Ana <ana@x.com>\r\nTo: a@x.com\r\nSubject: Our logo\r\nMessage-ID: <logo@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"rel\"\r\n\r\n--rel\r\nContent-Type: text/html\r\n\r\n<p>Here: <img src=\"cid:logo@x\"></p>\r\n--rel\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\nContent-ID: <logo@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--rel--\r\n",
            base64::engine::general_purpose::STANDARD.encode(&logo)
        );
        let p = crate::mime::parse(raw.as_bytes(), false).unwrap();
        let original = shared.with_writer(|st| {
            let id = st.upsert_header("a", "INBOX", 9, flags::SEEN, raw.len() as u32, 0, &p)?;
            st.set_body(id, raw.as_bytes(), &p)?;
            Ok(id)
        })
        .unwrap();
        let body = shared.with_writer(|st| st.body(original)).unwrap().unwrap();
        assert!(body.attachments.is_empty(), "the reader shows the picture in the message, not as a file");
        assert_eq!(body.inline.iter().map(|a| (a.index, a.filename.as_str())).collect::<Vec<_>>(), [(0, "image-1.png")]);
        type_draft(&shared, 7, 1, "Fwd: Our logo");
        let inline = &body.inline[0];
        handle(&shared, &mut HashMap::new(), Command::AttachForwarded { draft: 7, message: original, files: vec![(forwarded(5, &inline.filename), inline.index)] }).await.unwrap();
        wait_for(&rx, 1, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. }));
        assert_eq!(shared.with_writer(|st| st.attached_files(7)).unwrap()[0].1, logo, "a forward, written as plain text, carries it as a file");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_forwarded_message_not_downloaded_yet_is_fetched_first() {
        let (shared, rx) = drafts_engine();
        let (original, raw) = mail_with_files(&shared, 3, true);
        type_draft(&shared, 7, 1, "Fwd: The plan");
        // No connection here to fetch it on: said so, not left waiting.
        handle(&shared, &mut HashMap::new(), Command::AttachForwarded { draft: 7, message: original, files: vec![(forwarded(10, "Plán.pdf"), 0)] }).await.unwrap();
        assert!(matches!(wait_for(&rx, 1, |e| matches!(e, Event::AttachFailed { .. })).last(), Some(Event::AttachFailed { file: 10, .. })));
        // With one, the download's answer attaches them.
        handle(&shared, &mut HashMap::new(), Command::AttachForwarded { draft: 7, message: original, files: vec![(forwarded(11, "Plán.pdf"), 0), (forwarded(12, "map.png"), 1)] }).await.unwrap();
        wait_for(&rx, 2, |e| matches!(e, Event::AttachFailed { .. }));
        let pending = shared.with_writer(|st| st.attaching_files()).unwrap();
        assert!(pending.is_empty(), "nothing left waiting: {pending:?}");
        let p = crate::mime::parse(&raw, false).unwrap();
        shared.with_writer(|st| st.set_body(original, &raw, &p)).unwrap();
        let listed = vec![(forwarded(13, "Plán.pdf"), FileSource::Message { id: original, index: 0 }), (forwarded(14, "map.png"), FileSource::Message { id: original, index: 1 })];
        shared.with_writer(|st| st.attaching(7, &listed)).unwrap();
        reply(&shared, &Reply::Attach { message: original, files: vec![(13, 0), (14, 1)] }).unwrap();
        let files = attached(&wait_for(&rx, 2, |e| matches!(e, Event::FileAttached { .. })));
        assert_eq!(files.iter().map(|f| f.filename.as_str()).collect::<Vec<_>>(), ["Plán.pdf", "map.png"]);
        assert_eq!(shared.with_writer(|st| st.attached_files(7)).unwrap().last().unwrap().1, b"\x89PNG map".to_vec());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_a_restart_finds_half_attached_are_taken_up_or_said_lost() {
        let (shared, rx) = drafts_engine();
        let here = file_at("restart", "here.pdf", PDF);
        let gone = here.with_file_name("gone.pdf");
        let (original, _) = mail_with_files(&shared, 3, false);
        type_draft(&shared, 7, 1, "plan");
        // As the last run left them: listed, being attached, when the app quit.
        let listed = vec![
            (forwarded(20, "here.pdf"), FileSource::Path(here)),
            (forwarded(21, "gone.pdf"), FileSource::Path(gone)),
            (forwarded(22, "map.png"), FileSource::Message { id: original, index: 1 }),
        ];
        shared.with_writer(|st| st.attaching(7, &listed)).unwrap();
        resume_attaching(&shared, &HashMap::new());
        let events = wait_for(&rx, 3, |e| matches!(e, Event::FileAttached { .. } | Event::AttachFailed { .. }));
        assert_eq!(attached(&events).iter().map(|f| f.id).collect::<HashSet<_>>(), HashSet::from([20, 22]), "{events:?}");
        let files = shared.with_writer(|st| st.files_of(7)).unwrap();
        assert!(matches!(&files[1].status, FileStatus::Failed(why) if why.starts_with("Couldn't read gone.pdf")), "never dropped without a word: {files:?}");
        assert_eq!(uploaded_files(&shared).await.len(), 2);
    }

    #[tokio::test]
    async fn a_message_over_the_limit_is_refused_before_it_goes() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "big");
        // The server hasn't said what it takes: 25 MB of attachments.
        let big = vec![7u8; crate::attach::LIMIT + 1];
        shared.with_writer(|st| st.attaching(7, &[(forwarded(1, "video.mov"), FileSource::Path("video.mov".into()))])).unwrap();
        shared.with_writer(|st| st.file_in(1, &big, "", now())).unwrap();
        let err = refused(&shared, &rx, 7, as_shown(&shared, 7, draft("big"))).await;
        assert!(err.starts_with("Attachments total 25.0 MB, over the 25 MB limit"), "{err}");
        assert_eq!(shared.with_writer(|st| st.pending_count()).unwrap(), 1, "nothing queued but the draft's upload");
    }

    #[tokio::test]
    async fn a_message_too_big_for_its_server_is_refused_before_it_goes() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "big");
        let file = vec![7u8; 300_000];
        shared.with_writer(|st| st.attaching(7, &[(forwarded(1, "scan.pdf"), FileSource::Path("scan.pdf".into()))])).unwrap();
        shared.with_writer(|st| st.file_in(1, &file, "", now())).unwrap();
        // What the file alone takes, encoded: the composer can't tell it's over, the message
        // itself, built, is.
        let body = draft("big").body.len() + draft("big").subject.len();
        let fits = (crate::attach::encoded_len(file.len()) + body) as u64;
        shared.with_writer(|st| st.set_send_limit("a", fits)).unwrap();
        assert_eq!(shared.limit("a").over(&[file.len()], body), None);
        let big = as_shown(&shared, 7, draft("big"));
        let err = refused(&shared, &rx, 7, big.clone()).await;
        assert!(err.contains("once encoded for sending; your mail server takes at most"), "{err}");
        // Smaller still: the composer's rule catches it too, with the same words the engine
        // refuses it with.
        shared.with_writer(|st| st.set_send_limit("a", 100_000)).unwrap();
        let said = shared.limit("a").over(&[file.len()], body).unwrap();
        assert_eq!(refused(&shared, &rx, 7, big.clone()).await, said);
        // Room enough: it goes.
        shared.with_writer(|st| st.set_send_limit("a", 35_882_577)).unwrap();
        handle(&shared, &mut HashMap::new(), Command::Send { draft: big, draft_id: Some(7) }).await.unwrap();
        assert_eq!(queued_sends(&shared), 1);
    }

    fn queued_sends(shared: &Shared) -> usize {
        shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().filter(|q| matches!(q.op, Op::Send { .. })).count()
    }

    #[tokio::test]
    async fn a_send_missing_a_file_its_composer_shows_is_refused() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "plan");
        shared.with_writer(|st| st.attaching(7, &[(forwarded(500, "plan.pdf"), FileSource::Path("plan.pdf".into()))])).unwrap();
        shared.with_writer(|st| st.file_in(500, PDF, "", now())).unwrap();
        let shown = shared.with_writer(|st| st.draft(7)).unwrap().unwrap().draft;
        // Discarded and purged before a refused save carried its text on as draft 8: the file
        // went with it, while the composer still shows it.
        shared.with_writer(|st| st.discard_draft(7, None)).unwrap();
        handle(&shared, &mut HashMap::new(), Command::SaveDraftFrom { id: 8, draft: shown.clone(), open: true, rev: 2, lifecycle: 0, files_from: 7 }).await.unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        let told = events.iter().find_map(|e| if let Event::DraftFiles { draft: 8, files } = e { Some(files.clone()) } else { None }).expect("draft 8's files, as they are");
        assert_eq!(told.iter().map(|f| (f.id, f.status.clone())).collect::<Vec<_>>(), [(500, FileStatus::Failed("Lost with the draft it was attached to: attach it again".into()))], "kept, saying why");
        let err = refused(&shared, &rx, 8, shown).await;
        assert_eq!(err, "plan.pdf isn't attached: remove it, or attach it again");
        assert_eq!(queued_sends(&shared), 0, "never sent without a file it shows");
    }

    #[tokio::test]
    async fn a_send_from_a_composer_that_never_saw_a_file_is_refused() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "plan");
        let shown = shared.with_writer(|st| st.draft(7)).unwrap().unwrap().draft;
        // Another window attaches a file to the same draft.
        shared.with_writer(|st| st.attaching(7, &[(forwarded(500, "secret.pdf"), FileSource::Path("secret.pdf".into()))])).unwrap();
        shared.with_writer(|st| st.file_in(500, PDF, "", now())).unwrap();
        handle(&shared, &mut HashMap::new(), Command::Send { draft: shown.clone(), draft_id: Some(7) }).await.unwrap();
        assert_eq!(queued_sends(&shared), 0, "never sent with a file it doesn't show");
        let events: Vec<Event> = rx.try_iter().collect();
        let error = events.iter().find_map(|e| if let Event::SendFailed { draft_id: Some(7), error } = e { Some(error.clone()) } else { None });
        assert_eq!(error.as_deref(), Some("Not sent: its attachments changed (added: secret.pdf). Check them and send again."));
        // Then what's there, so the composer shows it: sent again, it goes.
        let files = events.iter().find_map(|e| if let Event::DraftFiles { draft: 7, files } = e { Some(files.clone()) } else { None }).expect("the files as they are");
        assert_eq!(files.iter().map(|f| f.id).collect::<Vec<_>>(), [500]);
        handle(&shared, &mut HashMap::new(), Command::Send { draft: Draft { attachments: files, ..shown }, draft_id: Some(7) }).await.unwrap();
        assert_eq!(queued_sends(&shared), 1);
    }

    #[tokio::test]
    async fn a_carried_on_draft_hears_its_files_as_they_are() {
        let (shared, rx) = drafts_engine();
        type_draft(&shared, 7, 1, "plan");
        shared.with_writer(|st| st.attaching(7, &[(forwarded(500, "slow.pdf"), FileSource::Path("slow.pdf".into()))])).unwrap();
        let shown = shared.with_writer(|st| st.draft(7)).unwrap().unwrap().draft;
        // The file comes in under draft 7 just before a refused save carries it on as draft 8:
        // the composer, on 8 by then, must still hear it's attached.
        shared.with_writer(|st| st.file_in(500, PDF, "", now())).unwrap();
        handle(&shared, &mut HashMap::new(), Command::SaveDraftFrom { id: 8, draft: shown, open: true, rev: 2, lifecycle: 0, files_from: 7 }).await.unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        let told = events.iter().find_map(|e| if let Event::DraftFiles { draft: 8, files } = e { Some(files.clone()) } else { None });
        assert_eq!(told.map(|f| f.into_iter().map(|f| (f.id, f.status)).collect::<Vec<_>>()), Some(vec![(500, FileStatus::Attached)]), "{events:?}");
    }

    #[tokio::test]
    async fn a_message_its_server_wont_take_comes_back_and_isnt_tried_again() {
        let (shared, rx) = drafts_engine();
        shared.settings.write().unwrap().set("compose.undoSendSeconds", serde_json::json!(0)).unwrap();
        type_draft(&shared, 7, 1, "big");
        handle(&shared, &mut HashMap::new(), Command::Send { draft: draft("big"), draft_id: Some(7) }).await.unwrap();
        // The server says, at send time, it takes less than that: before a byte of it goes.
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.too_big = Some(1000);
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(queued_sends(&shared), 0, "off the queue: no retry changes it");
        let events: Vec<Event> = rx.try_iter().collect();
        let back = events.iter().find_map(|e| if let Event::DraftOpened { id: 7, notice: Some(DraftNotice::NotSent(why)), .. } = e { Some(why.clone()) } else { None });
        assert!(back.as_deref().is_some_and(|w| w.starts_with("Not sent: this message is") && w.contains("takes at most 1000 B")), "back in the composer, saying why: {events:?}");
        let d = shared.with_writer(|st| st.draft(7)).unwrap().unwrap();
        assert!(d.visible && d.draft.subject == "big", "and in Drafts");
        assert_eq!(shared.with_writer(|st| st.send_limit("a")).unwrap(), Some(1000), "what the server said is kept");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(!server.ran.iter().any(|op| matches!(op, Op::Send { .. })), "never sent");
    }

    #[tokio::test]
    async fn sending_waits_for_a_file_still_being_attached() {
        let (shared, _rx) = drafts_engine();
        type_draft(&shared, 7, 1, "x");
        shared.with_writer(|st| st.attaching(7, &[(forwarded(1, "slow.mov"), FileSource::Path("slow.mov".into()))])).unwrap();
        let (shared, rx) = (shared, _rx);
        assert_eq!(refused(&shared, &rx, 7, as_shown(&shared, 7, draft("x"))).await, "Still attaching slow.mov…");
    }

    #[test]
    fn archiving_in_the_archive_queues_nothing() {
        let st = setup();
        let a = message(&st, "Archive", 1, "<1@x>", "", 0);
        let moved = move_local(&st, &[a], &MoveTarget::Archive, Some((1_000, 5))).unwrap();
        assert!(moved.touched.is_empty() && moved.held == 0);
        assert_eq!(visible(&st, "Archive"), vec![a]);
    }

    // ── snoozes ──

    /// [`setup`], with snoozes going to `Snoozed` and a bucket for each time in `buckets`.
    fn snooze_setup(buckets: &[i64]) -> Store {
        let st = setup();
        st.set_snooze_parent("a", "Snoozed", "/").unwrap();
        for wake in buckets {
            st.upsert_mailbox("a", &snooze::bucket("Snoozed", "/", *wake), "Snoozed", Role::Snoozed, "/", true).unwrap();
        }
        st
    }

    /// Three hours ahead, to the minute: a snooze that goes into its bucket.
    fn later() -> i64 {
        (now() + 3 * 3600) / 60 * 60
    }

    fn bucket_of(wake: i64) -> String {
        snooze::bucket("Snoozed", "/", wake)
    }

    fn queued(shared: &Shared) -> Vec<Op> {
        shared.with_writer(|st| st.ops("a", i64::MAX)).unwrap().into_iter().map(|q| q.op).collect()
    }

    #[test]
    fn a_snooze_is_a_held_move_into_its_bucket_and_undo_leaves_the_server_alone() {
        let st = snooze_setup(&[]);
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        st.set_counts("a", "INBOX", 1, 1).unwrap();
        let (shared, rx) = engine_with(st);
        let until = later();
        snooze_and_hold(&shared, vec![a], until, false, Some(9), 1000).unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, Event::Held { undo: 9, ops: 1 })));
        assert_eq!(queued(&shared), [Op::Move { mailbox: "INBOX".into(), uid: 1, dest: bucket_of(until) }]);
        shared
            .with_writer(|st| {
                assert!(st.claim_due("a", 1005)?.is_empty(), "held for the undo window");
                st.unclaim("a")?;
                assert!(visible(st, "INBOX").is_empty(), "gone from the inbox at once");
                assert_eq!(st.mailboxes("a")?[0].unread, 0);
                st.undo_batch(9)?;
                assert_eq!(visible(st, "INBOX"), vec![a], "back, as it was");
                assert_eq!(st.pending_count()?, 0, "nothing for the server");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_conversation_snoozes_whole_and_each_account_to_its_own_bucket() {
        let st = snooze_setup(&[]);
        st.set_snooze_parent("b", "INBOX.Snoozed", ".").unwrap();
        let first = message(&st, "INBOX", 1, "<1@x>", "", flags::SEEN);
        let reply = message(&st, "INBOX", 2, "<2@x>", "<1@x>", 0);
        let theirs = message_in(&st, "b", "INBOX", 1, "<3@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        let until = later();
        let touched = snooze_and_hold(&shared, vec![reply, theirs], until, true, Some(4), now()).unwrap();
        assert_eq!(touched, HashSet::from(["a".to_owned(), "b".to_owned()]));
        let dests = |account: &str| shared.with_writer(|st| st.ops(account, i64::MAX)).unwrap().into_iter().map(|q| q.op).collect::<Vec<_>>();
        assert_eq!(dests("a").len(), 2, "the whole conversation");
        assert!(dests("a").iter().all(|op| matches!(op, Op::Move { dest, .. } if *dest == bucket_of(until))));
        assert_eq!(dests("b"), [Op::Move { mailbox: "INBOX".into(), uid: 1, dest: snooze::bucket("INBOX.Snoozed", ".", until) }], "its own parent and delimiter");
        let _ = first;
    }

    #[tokio::test]
    async fn a_snooze_reaches_the_server_by_its_bucket_made_first() {
        let st = snooze_setup(&[]);
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        let until = later();
        snooze_and_hold(&shared, vec![a], until, false, Some(9), now() - 10).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.buckets, [bucket_of(until)]);
        assert_eq!(server.ran, [Op::Move { mailbox: "INBOX".into(), uid: 1, dest: bucket_of(until) }]);
        shared
            .with_writer(|st| {
                assert!(st.summary(a)?.is_none(), "moved for good");
                assert_eq!(st.buckets("a")?, [(bucket_of(until), until)], "its bucket known here at once, for its wake");
                assert!(st.ops("a", i64::MAX)?.is_empty());
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_snooze_whose_time_comes_before_it_reaches_the_server_wakes_instead() {
        // Snoozed for half a minute (or the app was closed past its time): never into its bucket.
        let st = snooze_setup(&[]);
        let a = message(&st, "INBOX", 1, "<1@x>", "", flags::SEEN);
        st.set_counts("a", "INBOX", 0, 1).unwrap();
        let (shared, _rx) = engine_with(st);
        let soon = now() + 30;
        snooze_and_hold(&shared, vec![a], soon, false, Some(9), now() - 10).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.counts = (1, 1); // as the server has it once it's done
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.buckets.is_empty(), "no bucket made");
        assert_eq!(server.ran, [Op::SetFlags { mailbox: "INBOX".into(), uid: 1, add: flags::WOKEN, remove: flags::SEEN }], "left in the inbox, unread and new");
        shared
            .with_writer(|st| {
                assert_eq!(visible(st, "INBOX"), vec![a], "shown again");
                let s = st.summary(a)?.unwrap();
                assert!(!s.seen() && s.flags & flags::WOKEN != 0, "unread, pinned");
                assert_eq!(st.mailboxes("a")?[0].unread, 1);
                assert!(st.ops("a", i64::MAX)?.is_empty() && st.has_woken("a")?);
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn rescheduling_into_the_past_from_a_bucket_wakes_it_into_the_inbox() {
        let old = later();
        let st = snooze_setup(&[old]);
        let a = message(&st, &bucket_of(old), 1, "<1@x>", "", flags::SEEN);
        let (shared, _rx) = engine_with(st);
        snooze_and_hold(&shared, vec![a], now() + 20, false, Some(9), now() - 10).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert!(server.buckets.is_empty());
        assert_eq!(
            server.ran,
            [Op::SetFlags { mailbox: bucket_of(old), uid: 1, add: flags::WOKEN, remove: flags::SEEN }, Op::Move { mailbox: bucket_of(old), uid: 1, dest: "INBOX".into() }]
        );
        let (gone, remembered) = shared.with_writer(|st| Ok((st.summary(a)?.is_none(), st.has_woken("a")?))).unwrap();
        assert!(gone && remembered, "on its way to the inbox, where it lands pinned");
    }

    #[tokio::test]
    async fn rescheduling_moves_a_message_from_bucket_to_bucket() {
        let (old, new) = (later(), later() + 86_400);
        let st = snooze_setup(&[old]);
        let a = message(&st, &bucket_of(old), 1, "<1@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        snooze_and_hold(&shared, vec![a], new, false, None, now()).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        assert_eq!(server.buckets, [bucket_of(new)]);
        assert_eq!(server.ran, [Op::Move { mailbox: bucket_of(old), uid: 1, dest: bucket_of(new) }]);
        // To the time it has already: nothing to do.
        let b = shared.with_writer(|st| Ok(message(st, &bucket_of(old), 2, "<2@x>", "", 0))).unwrap();
        snooze_and_hold(&shared, vec![b], old, false, None, now()).unwrap();
        assert!(queued(&shared).is_empty());
    }

    #[tokio::test]
    async fn a_parent_the_server_wont_make_sends_snoozes_to_envoy_snoozed() {
        let st = snooze_setup(&[]);
        let a = message(&st, "INBOX", 1, "<1@x>", "", 0);
        let (shared, _rx) = engine_with(st);
        let until = later();
        snooze_and_hold(&shared, vec![a], until, false, None, now()).unwrap();
        let mut server = FakeServer::new(&shared, |_: &Shared| {});
        server.refuses = Some("Snoozed");
        replay_with(&shared, &account("a"), &mut server).await.unwrap();
        let moved = snooze::bucket("Envoy Snoozed", "/", until);
        assert_eq!(server.buckets, std::slice::from_ref(&moved));
        assert_eq!(server.ran, [Op::Move { mailbox: "INBOX".into(), uid: 1, dest: moved }]);
        assert_eq!(shared.with_writer(|st| st.snooze_parent("a")).unwrap(), ("Envoy Snoozed".to_owned(), "/".to_owned()), "and the next snooze goes there");
    }

    #[test]
    fn the_listing_decides_where_snoozes_go() {
        let st = setup();
        let mut native = listed("Snoozed", Role::Snoozed);
        native.native_snoozed = true;
        store_listing(&st, "a", &[listed("INBOX", Role::Inbox), native]).unwrap();
        assert_eq!(st.snooze_parent("a").unwrap().0, "Envoy Snoozed", "the server's own is never written into");
        assert_eq!(st.snooze_bucket("a", 1_794_297_600).unwrap(), "Envoy Snoozed/20261110T0800Z");
        // Before any listing: Snoozed, with the inbox's delimiter.
        st.upsert_mailbox("c", "INBOX", "Inbox", Role::Inbox, ".", true).unwrap();
        assert_eq!(st.snooze_bucket("c", 1_794_297_600).unwrap(), "Snoozed.20261110T0800Z");
    }

    /// The server's side of snoozes for [`snooze_cycle`]: what's in each bucket (Message-IDs),
    /// the inbox, and what was asked.
    #[derive(Default)]
    struct Buckets {
        boxes: HashMap<String, Vec<&'static str>>,
        inbox: Vec<&'static str>,
        woken: Vec<String>,
        cleaned: Vec<String>,
        /// Wakes are refused (NO) or the connection drops.
        refuse: bool,
        lost: bool,
    }

    impl SnoozeServer for Buckets {
        fn wake(&mut self, bucket: &str, _inbox: &str) -> impl std::future::Future<Output = Result<imap::Woken>> + Send {
            self.woken.push(bucket.to_owned());
            let r = if self.lost {
                Err(anyhow::Error::new(imap::Lost("MOVE: the connection closed".into())))
            } else if self.refuse {
                Err(anyhow::Error::new(async_imap::error::Error::No("[OVERQUOTA] full".into())))
            } else {
                let moved: Vec<&'static str> = self.boxes.get_mut(bucket).map(std::mem::take).unwrap_or_default();
                self.inbox.extend(&moved);
                Ok(imap::Woken { moved: moved.len(), message_ids: moved.iter().map(|m| m.to_string()).collect() })
            };
            async move { r }
        }

        fn cleanup(&mut self, bucket: &str, _inbox: &str) -> impl std::future::Future<Output = Result<bool>> + Send {
            self.cleaned.push(bucket.to_owned());
            let empty = self.boxes.get(bucket).is_none_or(Vec::is_empty);
            if empty {
                self.boxes.remove(bucket);
            }
            async move { Ok(empty) }
        }
    }

    #[tokio::test]
    async fn buckets_wake_when_their_time_comes_and_not_before() {
        let t = 1_800_000_000;
        let (past, soon) = (t - 3600, t + 600);
        let (shared, _rx) = engine_with(snooze_setup(&[past, soon]));
        let mut server = Buckets::default();
        server.boxes.insert(bucket_of(past), vec!["<1@x>", "<2@x>"]);
        server.boxes.insert(bucket_of(soon), vec!["<3@x>"]);
        let mut clock = SnoozeClock::default();
        let done = snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t).await.unwrap();
        assert_eq!(done.woke, [bucket_of(past)], "the one due, caught up on");
        assert_eq!(server.inbox, ["<1@x>", "<2@x>"]);
        assert!(shared.with_writer(|st| st.has_woken("a")).unwrap(), "remembered, to pin as they land");
        // The next round asks nothing of a bucket just woken; the other waits for its time.
        let done = snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t + 1).await.unwrap();
        assert!(done.woke.is_empty() && server.woken.len() == 1, "{:?}", server.woken);
        let buckets = shared.with_writer(|st| st.buckets("a")).unwrap();
        assert_eq!(clock.next(&buckets, true), Some(soon), "the worker waits until then");
        snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, soon - 1).await.unwrap();
        assert!(!server.woken.contains(&bucket_of(soon)), "a second early");
        let done = snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, soon).await.unwrap();
        assert_eq!(done.woke, [bucket_of(soon)]);
        assert_eq!(server.inbox, ["<1@x>", "<2@x>", "<3@x>"]);
    }

    #[tokio::test]
    async fn without_move_each_envoy_waits_its_own_jitter() {
        let t = 1_800_000_000;
        let (shared, _rx) = engine_with(snooze_setup(&[t]));
        let mut clock = SnoozeClock::default();
        let buckets = shared.with_writer(|st| st.buckets("a")).unwrap();
        let at = clock.next(&buckets, false).unwrap();
        assert!((t..=t + snooze::MAX_JITTER).contains(&at), "{}", at - t);
        assert_eq!(clock.next(&buckets, false), Some(at), "the same wait each time it's asked");
        assert_eq!(clock.next(&buckets, true), Some(t), "with MOVE, no wait");
        let mut server = Buckets::default();
        server.boxes.insert(bucket_of(t), vec!["<1@x>"]);
        if at > t {
            snooze_cycle(&shared, &account("a"), &mut server, &mut clock, false, at - 1).await.unwrap();
            assert!(server.woken.is_empty(), "not before its jitter");
        }
        snooze_cycle(&shared, &account("a"), &mut server, &mut clock, false, at).await.unwrap();
        assert_eq!(server.woken, [bucket_of(t)]);
    }

    #[tokio::test]
    async fn a_refused_wake_waits_a_minute_and_a_lost_connection_ends_the_round() {
        let t = 1_800_000_000;
        let (shared, _rx) = engine_with(snooze_setup(&[t - 60]));
        let mut server = Buckets { refuse: true, ..Buckets::default() };
        server.boxes.insert(bucket_of(t - 60), vec!["<1@x>"]);
        let mut clock = SnoozeClock::default();
        snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t).await.unwrap();
        let buckets = shared.with_writer(|st| st.buckets("a")).unwrap();
        assert_eq!(clock.next(&buckets, true), Some(t + WAKE_RETRY), "no busy loop on a refusal");
        server.lost = true;
        assert!(snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t + WAKE_RETRY).await.is_err());
    }

    #[tokio::test]
    async fn only_empty_buckets_a_day_past_their_time_are_deleted() {
        let t = 1_800_000_000;
        let (gone, full, young) = (t - snooze::KEEP_EMPTY - 60, t - snooze::KEEP_EMPTY - 120, t - snooze::KEEP_EMPTY + 3600);
        let (shared, _rx) = engine_with(snooze_setup(&[gone, full, young]));
        let mut server = Buckets::default();
        server.boxes.insert(bucket_of(full), vec!["<late@x>"]);
        let mut clock = SnoozeClock::default();
        // Not woken yet (the app was off for a day): woken in the same round, not deleted.
        let done = snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t).await.unwrap();
        assert_eq!(done.deleted, [bucket_of(gone)]);
        let mut cleaned = server.cleaned.clone();
        cleaned.sort();
        assert_eq!(cleaned, [bucket_of(full), bucket_of(gone)], "a young one isn't even asked about");
        assert!(done.woke.contains(&bucket_of(full)));
        let left: Vec<String> = shared.with_writer(|st| st.buckets("a")).unwrap().into_iter().map(|(b, _)| b).collect();
        assert_eq!(left, [bucket_of(full), bucket_of(young)]);
        // Looked for again only every ten minutes.
        snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t + 60).await.unwrap();
        assert_eq!(server.cleaned.len(), 2);
        let done = snooze_cycle(&shared, &account("a"), &mut server, &mut clock, true, t + SNOOZE_RECHECK).await.unwrap();
        assert_eq!(done.deleted, [bucket_of(full)], "emptied by its wake, then deleted");
    }

    #[test]
    fn woken_messages_are_pinned_atop_the_inbox_until_dealt_with() {
        let st = snooze_setup(&[]);
        let newest = message(&st, "INBOX", 9, "<9@x>", "", 0);
        let woken = message(&st, "INBOX", 1, "<1@x>", "", flags::WOKEN);
        let middle = message(&st, "INBOX", 5, "<5@x>", "", 0);
        let reply = message(&st, "INBOX", 7, "<7@x>", "<1@x>", 0);
        let inbox = View::Role { account: None, role: Role::Inbox };
        let rows = |st: &Store, query: &str, conversations: bool| st.list(&inbox, &Query::parse(query), conversations, 100, &HashSet::new()).unwrap().into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(rows(&st, "", false), [woken, newest, reply, middle], "first, though older");
        assert_eq!(rows(&st, "", true)[0], woken, "its conversation first too");
        assert_eq!(rows(&st, "", true).len(), 3);
        assert_eq!(visible(&st, "INBOX")[0], woken, "in the mailbox's own view");
        assert_eq!(rows(&st, "from:ana", false), [newest, reply, middle, woken], "a search goes by date");
        // Dealt with: in its place by date.
        let (shared, _rx) = engine_with(st);
        set_flags(&shared, vec![woken], 0, flags::WOKEN, false).unwrap();
        assert_eq!(shared.with_writer(|st| Ok(rows(st, "", false))).unwrap(), [newest, reply, middle, woken]);
        assert_eq!(queued(&shared), [Op::SetFlags { mailbox: "INBOX".into(), uid: 1, add: 0, remove: flags::WOKEN }], "and the server told");
    }

    #[test]
    fn moving_a_woken_message_away_unpins_it_first() {
        let st = snooze_setup(&[]);
        let woken = message(&st, "INBOX", 1, "<1@x>", "", flags::WOKEN);
        st.add_woken("a", &["<1@x>".to_owned()], 0).unwrap();
        move_local(&st, &[woken], &MoveTarget::Archive, Some((1_000, 3))).unwrap();
        let due: Vec<Op> = st.ops("a", i64::MAX).unwrap().into_iter().map(|q| q.op).collect();
        assert_eq!(due, [Op::SetFlags { mailbox: "INBOX".into(), uid: 1, add: 0, remove: flags::WOKEN }, Op::Move { mailbox: "INBOX".into(), uid: 1, dest: "Archive".into() }]);
        assert!(!st.has_woken("a").unwrap());
    }

    #[test]
    fn the_snoozed_view_lists_every_bucket_soonest_first() {
        let (soon, late) = (later(), later() + 86_400);
        let st = snooze_setup(&[late, soon]);
        let older_but_later = message(&st, &bucket_of(late), 1, "<1@x>", "", 0);
        let newer_but_sooner = message(&st, &bucket_of(soon), 8, "<8@x>", "", 0);
        let also_sooner = message(&st, &bucket_of(soon), 2, "<2@x>", "", 0);
        let view = View::Role { account: None, role: Role::Snoozed };
        let rows: Vec<MessageId> = st.list(&view, &Query::default(), false, 100, &HashSet::new()).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(rows, [newer_but_sooner, also_sooner, older_but_later]);
        // Streamed or not, the same order: it comes in one piece.
        let mut first = Vec::new();
        let paged = st.list_paged(&view, &Query::default(), false, 100, &HashSet::new(), 1, &mut |p| first = p).unwrap();
        assert!(first.is_empty());
        assert_eq!(paged.into_iter().map(|s| s.id).collect::<Vec<_>>(), rows);
    }
}
