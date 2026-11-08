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
use crate::model::{Draft, DraftNotice, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};
use crate::settings::Settings;
use crate::store::{DraftRev, LocalDraft, Op, Query, Store};
use crate::{drafts, oauth, paths, secrets, smtp};

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
    List { req: u64, view: View, query: String, conversations: bool, limit: u32, stream: bool },
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
    /// Takes back a [`Command::Move`] (or [`Command::DiscardDraft`]) that hasn't reached the
    /// server yet.
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
    OpenOriginal { id: MessageId },
    Settings(Box<Settings>),
    /// Download (or read from the message, for `cid:`) and decode an image in an HTML mail.
    FetchImage { url: String, message: MessageId },
}

#[derive(Debug, Clone)]
pub enum Event {
    Accounts(Vec<Account>),
    Mailboxes { account: String, mailboxes: Vec<Mailbox> },
    /// A list's rows. `more`: these are the top of a longer list, whose whole (starting with
    /// the same rows) follows in another `List` with the same `req`.
    List { req: u64, items: Vec<Summary>, more: bool },
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
    SendFailed { draft_id: Option<i64> },
    /// Save `rev` of draft `id` is written to the local database ([`Command::SaveDraft`]).
    DraftSaved { id: i64, rev: i64 },
    /// Save `rev` of draft `id` couldn't be written: it isn't saved, and should be tried again.
    DraftSaveFailed { id: i64, rev: i64, error: String },
    /// Save `rev` of draft `id` was refused: the draft moved on without it (gone for good, or
    /// sent, since it was made). It isn't saved, and a retry won't be: keep it as a new draft.
    DraftSaveRefused { id: i64, rev: i64 },
    /// How many ops an undoable [`Command::Move`] actually held. 0 means it had nothing to do
    /// (the messages were already moving, or already there), so there's nothing to undo.
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
}

type Waker = Arc<dyn Fn() + Send + Sync>;

/// How many rows of a long list are sent ahead of the rest: more than a screenful, and what
/// the list mounts around it, but a small part of the work.
pub const FIRST_PAGE: u32 = 100;

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
                undo_keys: Mutex::new(HashMap::new()),
            });
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
    pub fn save_draft_now(&self, id: i64, draft: &Draft, open: bool, at: DraftRev) -> Result<bool> {
        let st = self.writer.lock()?;
        Ok(drafts::save(&st, id, draft, open, at, now())?.is_some())
    }

    /// Keeps a message the composer sent (at `at`) as a draft, on the calling thread, unless
    /// its send was queued (or went out): for when the window closes before the engine
    /// confirmed it. False when the store refused it ([`Store::draft_unsent`]).
    pub fn keep_unsent_now(&self, id: i64, draft: &Draft, at: DraftRev) -> Result<bool> {
        let st = self.writer.lock()?;
        st.draft_unsent(id, draft, Some(at), now())
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
    /// Mailboxes being looked at, `(account, name)`: synced every cycle.
    watched: Mutex<HashSet<(String, String)>>,
    /// Undo keys of moves this run held ops for, with how many: to tell "too late" from
    /// "nothing to undo", and a partial undo from a full one.
    undo_keys: Mutex<HashMap<i64, usize>>,
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
    /// Download these messages now (someone opened them), then answer.
    Bodies { mailbox: String, uids: Vec<u32>, reply: Option<Reply> },
    SyncMailbox(String),
    Sync,
}

/// What to send the UI once [`Job::Bodies`] has the messages.
#[derive(Debug)]
enum Reply {
    /// The open conversation again: `(req, id, conversations)` as in [`Command::Open`].
    Thread(u64, MessageId, bool),
    /// A Drafts row, now readable, in the composer ([`Command::OpenDraft`]).
    Draft(MessageId),
}

/// Answers a [`Job::Bodies`] once its messages are downloaded.
fn reply(shared: &Shared, reply: &Reply) -> Result<()> {
    match *reply {
        Reply::Thread(req, id, conversations) => {
            let messages = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?.thread(id, conversations)?;
            shared.emit(Event::Thread { req, id, messages });
        }
        Reply::Draft(id) => match open_draft(shared, id, now())? {
            Opening::Ready(d, notice) => shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: false, notice }),
            Opening::Copy(draft, why) => shared.emit(Event::DraftOpened { id: 0, draft: *draft, at: DraftRev::default(), restored: false, notice: Some(DraftNotice::Copy(why)) }),
            _ => {}
        },
    }
    Ok(())
}

async fn run(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Command>) {
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
    match shared.with_writer(|st| st.draft_to_restore()) {
        Ok(Some(d)) => shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: true, notice: None }),
        Ok(None) => {}
        Err(e) => log::warn!("couldn't read drafts: {e:#}"),
    }
    shared.emit_pending();
    tokio::spawn(upgrade_search(shared.clone()));
    while let Some(cmd) = rx.recv().await {
        if let Err(e) = handle(&shared, &mut workers, cmd).await {
            shared.emit(Event::Error(format!("{e:#}")));
        }
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

fn poke(workers: &HashMap<String, Worker>, account: &str, job: Job) {
    if let Some(w) = workers.get(account) {
        match job {
            Job::Bodies { .. } | Job::SyncMailbox(_) => {
                let _ = w.now.send(job);
            }
            Job::Sync => {
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
        Command::List { req, view, query, conversations, limit, stream } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let me = shared.me();
                let q = Query::parse(&query);
                let items = shared.reader.lock().map_err(|_| anyhow!("store poisoned")).and_then(|st| {
                    // The top of a long list goes out first, and the rest when it's read. A
                    // Drafts list goes in one piece: its rows are merged with the local drafts.
                    let mut first = |items| shared.emit(Event::List { req, items, more: true });
                    let page = if stream && !drafts::lists_drafts(&st, &view) { FIRST_PAGE } else { 0 };
                    let items = st.list_paged(&view, &q, conversations, limit, &me, page, &mut first)?;
                    drafts::merge(&st, &view, &q, limit, items, |a| shared.sender(a))
                });
                match items {
                    Ok(items) => shared.emit(Event::List { req, items, more: false }),
                    Err(e) => shared.emit(Event::Error(format!("search failed: {e}"))),
                }
            });
        }
        Command::Open { req, id, conversations } => {
            let messages = {
                let st = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?;
                st.thread(id, conversations)?
            };
            // Ask for any bodies we don't have, grouped by mailbox.
            let mut missing: HashMap<(String, String), Vec<u32>> = HashMap::new();
            for m in messages.iter().filter(|m| m.body.is_none()) {
                missing.entry((m.summary.account.clone(), m.summary.mailbox.clone())).or_default().push(m.summary.uid);
            }
            shared.emit(Event::Thread { req, id, messages });
            for ((account, mailbox), uids) in missing {
                poke(workers, &account, Job::Bodies { mailbox, uids, reply: Some(Reply::Thread(req, id, conversations)) });
            }
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
        Command::UndoMove { undo } => undo_move(shared, undo)?,
        Command::Send { draft, draft_id } => {
            if let Err(e) = queue_send(shared, workers, &draft, draft_id) {
                // Not sent: kept as a draft here, and back in the composer, so nothing written is
                // lost.
                if let Some(id) = draft_id
                    && let Err(e) = shared.with_writer(|st| st.draft_unsent(id, &draft, None, now()))
                {
                    log::warn!("couldn't keep an unsent draft: {e:#}");
                }
                shared.emit(Event::SendFailed { draft_id });
                return Err(e);
            }
        }
        Command::UndoSend { op } => {
            // The draft comes back in the composer, the same one: Drafts doesn't get a second.
            let (cancelled, back) = shared.with_writer(|st| {
                st.transaction(|st| {
                    let cancelled = st.cancel_send(op)?;
                    Ok((cancelled, if cancelled { st.unsend_draft(op, now())? } else { None }))
                })
            })?;
            shared.emit(Event::Toast(if cancelled { "Sending undone".into() } else { "Too late — it was already sent".into() }));
            if let Some(d) = back {
                let account = d.account.clone();
                shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: false, notice: None });
                shared.emit(Event::Changed { account: account.clone() });
                wake(workers, &account);
            }
            shared.emit_pending();
        }
        Command::SaveDraft { id, draft, open, rev, lifecycle } => match shared.with_writer(|st| drafts::save(st, id, &draft, open, DraftRev { lifecycle, rev }, now())) {
            Ok(Some(queued)) => {
                // Only now is it saved: the app shows "Draft saved", and drops its own copy.
                shared.emit(Event::DraftSaved { id, rev });
                if queued {
                    shared.emit_pending();
                    wake(workers, &draft.account);
                }
            }
            Ok(None) => shared.emit(Event::DraftSaveRefused { id, rev }),
            Err(e) => shared.emit(Event::DraftSaveFailed { id, rev, error: format!("{e:#}") }),
        },
        Command::OpenDraft { id } => match open_draft(shared, id, now())? {
            Opening::Ready(d, notice) => shared.emit(Event::DraftOpened { id: d.id, draft: d.draft, at: d.at, restored: false, notice }),
            Opening::Copy(draft, why) => shared.emit(Event::DraftOpened { id: 0, draft: *draft, at: DraftRev::default(), restored: false, notice: Some(DraftNotice::Copy(why)) }),
            Opening::Download { account, mailbox, uid } => poke(workers, &account, Job::Bodies { mailbox, uids: vec![uid], reply: Some(Reply::Draft(id)) }),
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
                let _ = w.jobs.send(Job::Sync);
                w.notify.notify_one();
                shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing("Checking".into()) });
            }
        }
        Command::Watch { view } => {
            let targets: Vec<(String, String)> = shared.with_writer(|st| {
                let accts: Vec<String> = shared.accounts.read().map(|a| a.iter().map(|a| a.id.clone()).collect()).unwrap_or_default();
                Ok(match &view {
                    View::Mailbox { account, mailbox } => vec![(account.clone(), mailbox.clone())],
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
                poke(workers, &account, Job::SyncMailbox(mailbox));
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
            shared.emit(Event::Accounts(list));
            shared.emit(Event::AccountAdded(account.clone()));
            workers.insert(account.id.clone(), spawn_worker(shared.clone(), account.id.clone()));
        }
        Command::RemoveAccount { id } => {
            if let Some(w) = workers.remove(&id) {
                w.task.abort();
                w.fg_task.abort();
            }
            secrets::remove(&id);
            let list = {
                let mut accts = shared.accounts.write().map_err(|_| anyhow!("poisoned"))?;
                accts.retain(|a| a.id != id);
                accounts::save(&accts)?;
                accts.clone()
            };
            shared.with_writer(|st| st.forget_account(&id))?;
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
            let body = shared.with_writer(|st| st.body(id))?.ok_or_else(|| anyhow!("The message isn't downloaded yet"))?;
            let dir = std::env::temp_dir().join("envoy");
            std::fs::create_dir_all(&dir)?;
            let path = match (&body.html, &body.text) {
                (Some(h), _) => {
                    let p = dir.join(format!("message-{id}.html"));
                    std::fs::write(&p, h)?;
                    p
                }
                (None, Some(t)) => {
                    let p = dir.join(format!("message-{id}.txt"));
                    std::fs::write(&p, t)?;
                    p
                }
                _ => return Err(anyhow!("This message has no body")),
            };
            shared.emit(Event::Saved(path));
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
            for w in workers.values() {
                w.notify.notify_one();
            }
        }
    }
    Ok(())
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
    // Never sent without a file the draft lists.
    let files = match draft_id {
        _ if draft.attachments.is_empty() => Vec::new(),
        Some(d) => shared.with_writer(|st| st.draft_files(d, &draft.attachments))?,
        None => return Err(anyhow!("The attached files aren't saved with this message")),
    };
    let built = smtp::build(&account, draft, &files)?;
    let undo = shared.setting_u32("compose.undoSendSeconds");
    let (sent_mailbox, reply_of) = shared.with_writer(|st| {
        let sent = if account.is_gmail() { None } else { st.mailbox_for_role(&account.id, Role::Sent)? };
        let reply = match draft.reply_of {
            Some(id) => st.location(id)?.map(|(_, m, u)| (m, u)),
            None => None,
        };
        Ok((sent, reply))
    })?;
    let op = Op::Send {
        raw_b64: base64::engine::general_purpose::STANDARD.encode(&built.raw),
        from: built.from,
        to: built.to,
        sent_mailbox,
        reply_of,
    };
    // The draft waits with the message (hidden from Drafts) and goes once it's out.
    let id = shared.with_writer(|st| {
        st.transaction(|st| {
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
    let window = shared.setting_u32("behavior.undoSeconds");
    // A second of grace on top of the window, so the toast's Undo button (shown for
    // exactly the window) never outlives the hold, whatever the clocks' rounding.
    let hold = undo.filter(|_| window > 0).map(|key| (now + window as i64 + 1, key));
    let mut moved = shared.with_writer(|st| st.transaction(|st| move_local(st, &ids, to, hold)))?;
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
    Ok(moved.touched)
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
    /// A server draft not downloaded yet: fetch it, then try again.
    Download { account: String, mailbox: String, uid: u32 },
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
                (Some(s), None) => Opening::Download { account: s.account, mailbox: s.mailbox, uid: s.uid },
                (Some(s), Some(raw)) => match crate::mime::server_draft(&raw) {
                    // Left as it is on the server: the copy becomes a draft of its own if edited.
                    Some(crate::mime::ServerDraft { draft, notice: Some(DraftNotice::Copy(why)), .. }) => Opening::Copy(Box::new(Draft { account: s.account, ..draft }), why),
                    Some(found) => st.adopt_draft(id, &found.draft, &found.files, now)?.map_or(Opening::Gone, |d| Opening::Ready(Box::new(d), found.notice)),
                    None => Opening::Gone,
                },
            },
        };
        if let Opening::Ready(d, _) = &opening {
            st.set_draft_open(d.id, true)?;
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
        let unread = st.summary(*id)?.is_some_and(|s| !s.seen());
        if *to == MoveTarget::Trash && here == Role::Trash {
            st.remove_local(*id)?;
            st.bump_counts(&account, &mailbox, if unread { -1 } else { 0 }, -1)?;
            st.enqueue(&account, &Op::Expunge { mailbox: mailbox.clone(), uid })?;
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
struct Moved {
    touched: HashSet<String>,
    problems: Vec<String>,
    /// Ops queued under the undo key.
    held: usize,
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

fn spawn_worker(shared: Arc<Shared>, account: String) -> Worker {
    let (jobs, rx) = mpsc::unbounded_channel();
    let (now, now_rx) = mpsc::unbounded_channel();
    let notify = Arc::new(Notify::new());
    let task = tokio::spawn(worker(shared.clone(), account.clone(), rx, notify.clone()));
    let fg_task = tokio::spawn(foreground(shared, account, now_rx, jobs.clone(), notify.clone()));
    Worker { jobs, now, notify, task, fg_task }
}

/// The foreground connection: opened on demand, closed after five idle minutes. If it can't
/// connect, its jobs fall back to the background loop, so nothing is lost.
async fn foreground(shared: Arc<Shared>, id: String, mut rx: mpsc::UnboundedReceiver<Job>, fallback: mpsc::UnboundedSender<Job>, notify: Arc<Notify>) {
    let mut session: Option<Session> = None;
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
        if session.is_none() {
            let connected = match shared.credential(&account).await {
                Ok(cred) => imap::connect(&account, &cred).await,
                Err(e) => Err(e),
            };
            match connected {
                Ok(s) => session = Some(s),
                Err(e) => {
                    log::warn!("{}: foreground connect failed: {e:#}", account.email);
                    let _ = fallback.send(job);
                    notify.notify_one();
                    continue;
                }
            }
        }
        let Some(s) = session.as_mut() else { continue };
        if let Err(e) = foreground_job(&shared, &account, s, &job).await {
            log::warn!("{}: {e:#}", account.email);
            session = None;
            let _ = fallback.send(job);
            notify.notify_one();
        }
    }
}

async fn foreground_job(shared: &Arc<Shared>, account: &Account, session: &mut Session, job: &Job) -> Result<()> {
    let id = &account.id;
    match job {
        Job::Bodies { mailbox, uids, reply: then } => {
            imap::select(session, mailbox).await?;
            imap::fetch_bodies(session, &shared.writer, id, mailbox, uids).await?;
            if let Some(then) = then {
                reply(shared, then)?;
            }
            shared.emit(Event::Changed { account: id.clone() });
        }
        Job::SyncMailbox(mailbox) => {
            let window = shared.setting_u32("sync.initialMessages").max(50);
            let progress = {
                let shared = shared.clone();
                let id = id.clone();
                move || shared.emit(Event::Changed { account: id.clone() })
            };
            let out = imap::sync_mailbox(session, &shared.writer, id, mailbox, window, &progress).await?;
            if let Ok((unread, total)) = imap::status(session, mailbox).await {
                shared.with_writer(|st| st.set_counts(id, mailbox, unread, total))?;
            }
            shared.emit_mailboxes(id);
            if out.changed {
                shared.emit(Event::Changed { account: id.clone() });
            }
            // Bodies for the top of the list, so the first messages opened are instant.
            let max_size = shared.setting_u32("sync.maxPrefetchSizeKb").saturating_mul(1024);
            let missing = shared.with_writer(|st| st.missing_bodies(id, mailbox, 25, max_size))?;
            if !missing.is_empty() {
                imap::select(session, mailbox).await?;
                imap::fetch_bodies(session, &shared.writer, id, mailbox, &missing).await?;
                shared.emit(Event::Changed { account: id.clone() });
            }
        }
        Job::Sync => {}
    }
    Ok(())
}

/// One account's sync loop: connect, catch up, replay queued changes, download ahead, then
/// wait (IMAP IDLE when available) until something changes. Reconnects with backoff.
async fn worker(shared: Arc<Shared>, id: String, mut jobs: mpsc::UnboundedReceiver<Job>, notify: Arc<Notify>) {
    let mut backoff = 2u64;
    let mut pending_jobs: Vec<Job> = Vec::new();
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
        match session_loop(&shared, &account, session, &mut jobs, &mut pending_jobs, &notify).await {
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
) -> Result<()> {
    let id = account.id.clone();
    let caps = imap::capabilities(&mut session).await?;
    let mut last_full: Option<Instant> = None;
    let mut last_list: Option<Instant> = None;
    let mut inbox_dirty = true;
    loop {
        while let Ok(j) = jobs.try_recv() {
            pending.push(j);
        }
        // Opened messages first: someone is waiting.
        let (bodies, rest): (Vec<Job>, Vec<Job>) = pending.drain(..).partition(|j| matches!(j, Job::Bodies { .. }));
        for job in bodies {
            if let Job::Bodies { mailbox, uids, reply: then } = job {
                imap::select(&mut session, &mailbox).await?;
                imap::fetch_bodies(&mut session, &shared.writer, &id, &mailbox, &uids).await?;
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
            shared.with_writer(|st| {
                for m in &listed {
                    st.upsert_mailbox(&id, &m.name, &m.display, m.role, &m.delimiter, m.selectable)?;
                }
                st.retain_mailboxes(&id, &listed.iter().map(|m| m.name.clone()).collect::<Vec<_>>())?;
                Ok(())
            })?;
            last_list = Some(Instant::now());
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
        for name in &targets {
            shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing(display_of(&mailboxes, name)) });
            let progress = {
                let shared = shared.clone();
                let id = id.clone();
                move || shared.emit(Event::Changed { account: id.clone() })
            };
            let out = imap::sync_mailbox(&mut session, &shared.writer, &id, name, window, &progress).await?;
            changed |= out.changed;
            if out.changed {
                shared.emit(Event::Changed { account: id.clone() });
            }
        }
        inbox_dirty = false;
        if full_due {
            for m in mailboxes.iter().filter(|m| m.selectable) {
                if let Ok((unread, total)) = imap::status(&mut session, &m.name).await {
                    shared.with_writer(|st| st.set_counts(&id, &m.name, unread, total))?;
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
                let missing = shared.with_writer(|st| st.missing_bodies(&id, &name, prefetch, max_size))?;
                if missing.is_empty() {
                    continue;
                }
                shared.emit(Event::Sync { account: id.clone(), state: SyncState::Syncing(format!("Downloading {}", missing.len())) });
                imap::select(&mut session, &name).await?;
                let batch: Vec<u32> = missing.iter().take(20).copied().collect();
                imap::fetch_bodies(&mut session, &shared.writer, &id, &name, &batch).await?;
                shared.emit(Event::Changed { account: id.clone() });
                more_to_fetch = missing.len() > batch.len();
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
            wait = wait.min(Duration::from_secs(d.max(1) as u64));
        }
        // Retry failed ops soon.
        let failed_due = shared.with_writer(|st| st.ops(&id, now() + 3600).map(|o| !o.is_empty()))?;
        if failed_due {
            wait = wait.min(Duration::from_secs(30));
        }
        if wait.is_zero() {
            continue;
        }
        let inbox = mailboxes.iter().find(|m| m.role == Role::Inbox).map(|m| m.name.clone());
        log::debug!("{}: waiting up to {wait:?}", account.email);
        if caps.idle && shared.setting_bool("sync.useIdle") && inbox.is_some() {
            imap::select(&mut session, inbox.as_deref().unwrap_or("INBOX")).await?;
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

fn display_of(mailboxes: &[Mailbox], name: &str) -> String {
    mailboxes.iter().find(|m| m.name == name).map_or_else(|| name.to_owned(), |m| m.display.clone())
}

async fn replay_ops(shared: &Arc<Shared>, account: &Account, session: &mut Session, caps: imap::Caps) -> Result<bool> {
    replay_with(shared, account, &mut ImapRunner { shared, account, session, caps }).await
}

/// Carries out queued ops on the server. A seam so replay can be tested without a server.
trait OpRunner {
    /// Every op but the draft ones, which replay through the two calls below.
    fn run(&mut self, op: &Op) -> impl std::future::Future<Output = Result<()>> + Send;
    /// The server's (unread, total) for a mailbox.
    fn status(&mut self, mailbox: &str) -> impl std::future::Future<Output = Result<(u32, u32)>> + Send;
    /// Puts a draft in `mailbox`, replacing its `previous` copy ([`imap::replace_draft`]).
    fn replace_draft(&mut self, mailbox: &str, raw: &[u8], message_id: &str, previous: Option<(u32, u32)>, maybe_strays: bool) -> impl std::future::Future<Output = Result<imap::Replaced>> + Send;
    /// Deletes a draft's copies from `mailbox`: `known`, and, with `search`, any with its
    /// Message-ID. Returns their UIDs.
    fn purge_draft(&mut self, mailbox: &str, message_id: &str, known: Option<(u32, u32)>, search: bool) -> impl std::future::Future<Output = Result<Vec<u32>>> + Send;
}

struct ImapRunner<'a> {
    shared: &'a Arc<Shared>,
    account: &'a Account,
    session: &'a mut Session,
    caps: imap::Caps,
}

impl OpRunner for ImapRunner<'_> {
    async fn status(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        imap::status(self.session, mailbox).await
    }

    async fn replace_draft(&mut self, mailbox: &str, raw: &[u8], message_id: &str, previous: Option<(u32, u32)>, maybe_strays: bool) -> Result<imap::Replaced> {
        imap::replace_draft(self.session, self.caps, mailbox, raw, message_id, previous, maybe_strays).await
    }

    async fn purge_draft(&mut self, mailbox: &str, message_id: &str, known: Option<(u32, u32)>, search: bool) -> Result<Vec<u32>> {
        imap::purge_message_id(self.session, self.caps, mailbox, message_id, known, search).await
    }

    async fn run(&mut self, op: &Op) -> Result<()> {
        let (shared, account, session) = (self.shared, self.account, &mut *self.session);
        match op {
            Op::SetFlags { mailbox, uid, add, remove } => imap::store_flags(session, mailbox, *uid, *add, *remove).await,
            Op::Move { mailbox, uid, dest } => imap::move_message(session, self.caps, mailbox, *uid, dest).await,
            Op::Expunge { mailbox, uid } => imap::expunge(session, self.caps, mailbox, *uid, true).await,
            Op::SaveDraft { .. } | Op::PurgeDraft { .. } => Err(anyhow!("draft ops replay through replay_draft")),
            Op::Send { raw_b64, from, to, sent_mailbox, reply_of } => {
                let raw = base64::engine::general_purpose::STANDARD.decode(raw_b64)?;
                let cred = shared.credential(account).await?;
                log::info!("{}: sending to {} recipient(s)", account.email, to.len());
                smtp::send(account, &cred, from, to, &raw).await?;
                log::info!("{}: sent", account.email);
                shared.emit(Event::Toast("Message sent".into()));
                // From here on the message is out; failures below must not resend it.
                if let Some(sent) = sent_mailbox
                    && let Err(e) = imap::append(session, sent, &raw, true).await
                {
                    log::warn!("couldn't save to Sent: {e:#}");
                }
                if let Some((mailbox, uid)) = reply_of {
                    let _ = imap::store_flags(session, mailbox, *uid, flags::ANSWERED, 0).await;
                }
                Ok(())
            }
        }
    }
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
    // Mailboxes moved out of: their counts get re-read once the pass is done.
    let mut recount: Vec<String> = Vec::new();
    for q in ops {
        let r = match &q.op {
            Op::SaveDraft { .. } | Op::PurgeDraft { .. } => replay_draft(shared, account, runner, &q.op).await,
            op => runner.run(op).await,
        };
        match r {
            Ok(()) => {
                shared.with_writer(|st| st.op_done(q.id))?;
                any = true;
                if let Op::Move { mailbox, .. } | Op::Expunge { mailbox, .. } = &q.op
                    && !recount.contains(mailbox)
                {
                    recount.push(mailbox.clone());
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let connection_lost = msg.contains("timed out") || msg.contains("connection") || msg.contains("Connection") || msg.contains("broken pipe");
                let draft_op = matches!(q.op, Op::SaveDraft { .. } | Op::PurgeDraft { .. });
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
                } else if q.attempts >= 5 && !connection_lost && !matches!(q.op, Op::Send { .. }) {
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
                if connection_lost {
                    return Err(e);
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
            let files = shared.with_writer(|st| st.draft_files(*draft, &up.draft.attachments)).map_err(|e| imap::Refused(format!("{e:#}")))?;
            let raw = smtp::build_draft(account, &up.draft, &up.message_id, &files).map_err(|e| imap::Refused(format!("{e:#}")))?;
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
        op => return runner.run(op).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            writer: Arc::new(Mutex::new(st)),
            reader: Arc::new(Mutex::new(Store::memory())),
            events: Mutex::new(tx),
            waker: Arc::new(|| {}),
            settings: RwLock::new(Settings::default()),
            tokens: Mutex::new(HashMap::new()),
            accounts: RwLock::new(vec![]),
            watched: Mutex::new(HashSet::new()),
            undo_keys: Default::default(),
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
            }
        }

        /// Runs `during` if this is the first op on the wire; a failing server's NO.
        fn on_the_wire(&mut self) -> Result<()> {
            if let Some(during) = self.during.take() {
                during(&self.shared);
            }
            if self.fail { Err(anyhow::Error::new(async_imap::error::Error::No(self.failure.into()))) } else { Ok(()) }
        }

        /// The drafts on the server, `(uid, subject)`.
        fn drafts(&self) -> Vec<(u32, String)> {
            self.drafts.iter().map(|(uid, _, raw)| (*uid, crate::mime::parse(raw, true).unwrap().subject)).collect()
        }
    }

    impl OpRunner for FakeServer {
        fn run(&mut self, op: &Op) -> impl std::future::Future<Output = Result<()>> + Send {
            self.ran.push(op.clone());
            let r = self.on_the_wire();
            async move { r }
        }

        fn status(&mut self, _: &str) -> impl std::future::Future<Output = Result<(u32, u32)>> + Send {
            let counts = self.counts;
            async move { Ok(counts) }
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
                Ok(_) if std::mem::take(&mut self.cut_off) => Err(anyhow!("connection lost")),
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

    #[tokio::test]
    async fn a_send_that_cant_be_queued_goes_back_to_the_composer() {
        let (shared, rx) = drafts_engine();
        let unsendable = Draft { account: "removed".into(), ..draft("unsendable") };
        assert!(handle(&shared, &mut HashMap::new(), Command::Send { draft: unsendable, draft_id: Some(9) }).await.is_err());
        assert!(rx.try_iter().any(|e| matches!(e, Event::SendFailed { draft_id: Some(9) })), "the composer gets it back");
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
        assert_eq!(shared.with_writer(|st| st.draft_files(d.id, &d.draft.attachments)).unwrap()[0].1, PDF.to_vec());
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

    #[test]
    fn archiving_in_the_archive_queues_nothing() {
        let st = setup();
        let a = message(&st, "Archive", 1, "<1@x>", "", 0);
        let moved = move_local(&st, &[a], &MoveTarget::Archive, Some((1_000, 5))).unwrap();
        assert!(moved.touched.is_empty() && moved.held == 0);
        assert_eq!(visible(&st, "Archive"), vec![a]);
    }
}
