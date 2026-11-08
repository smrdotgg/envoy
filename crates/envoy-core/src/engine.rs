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
use crate::model::{Draft, Mailbox, MessageId, Role, Summary, SyncState, ThreadMessage, View, flags};
use crate::settings::Settings;
use crate::store::{Op, Query, Store};
use crate::{oauth, paths, secrets, smtp};

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
    List { req: u64, view: View, query: String, conversations: bool, limit: u32 },
    Open { req: u64, id: MessageId, conversations: bool },
    /// `thread`: apply to every message of each id's conversation.
    SetFlags { ids: Vec<MessageId>, add: u32, remove: u32, thread: bool },
    /// `thread`: move each id's whole conversation (the part in the same mailbox).
    /// `undo`: a key the caller picks, unique across runs, to undo this action with
    /// [`Command::UndoMove`]; the move then waits `behavior.undoSeconds` before reaching the
    /// server. `None` sends it at once.
    Move { ids: Vec<MessageId>, to: MoveTarget, thread: bool, undo: Option<i64> },
    /// Takes back a [`Command::Move`] that hasn't reached the server yet.
    UndoMove { undo: i64 },
    Send { draft: Draft },
    UndoSend { op: i64 },
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
    List { req: u64, items: Vec<Summary> },
    Thread { req: u64, id: MessageId, messages: Vec<ThreadMessage> },
    /// Something in this account's local copy changed: reload what's on screen.
    Changed { account: String },
    Sync { account: String, state: SyncState },
    Toast(String),
    Error(String),
    Sent { op: i64, undo_secs: u32 },
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
}

type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Engine {
    tx: mpsc::UnboundedSender<Command>,
    rx: std::sync::mpsc::Receiver<Event>,
}

impl Engine {
    pub fn start(waker: impl Fn() + Send + Sync + 'static, settings: Settings) -> Result<Engine> {
        let (tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, rx) = std::sync::mpsc::channel();
        let db = paths::database_file();
        let writer = Arc::new(Mutex::new(Store::open(&db)?));
        let reader = Arc::new(Mutex::new(Store::open(&db)?));
        let shared = Arc::new(Shared {
            writer,
            reader,
            events: Mutex::new(ev_tx),
            waker: Arc::new(waker),
            settings: RwLock::new(settings),
            tokens: Mutex::new(HashMap::new()),
            accounts: RwLock::new(accounts::load()),
            watched: Mutex::new(HashSet::new()),
            undo_keys: Mutex::new(HashSet::new()),
        });
        std::thread::Builder::new().name("envoy-engine".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("envoy-io").build() {
                Ok(rt) => rt,
                Err(e) => {
                    shared.emit(Event::Error(format!("couldn't start the engine: {e}")));
                    return;
                }
            };
            rt.block_on(run(shared, cmd_rx));
        })?;
        Ok(Engine { tx, rx })
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
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
    /// Undo keys of moves this run held ops for (to tell "too late" from "nothing to undo").
    undo_keys: Mutex<HashSet<i64>>,
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
    /// Download these messages now (someone opened them), then re-send the thread.
    Bodies { mailbox: String, uids: Vec<u32>, reply: Option<(u64, MessageId, bool)> },
    SyncMailbox(String),
    Sync,
}

async fn run(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Command>) {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let accounts = shared.accounts.read().map(|a| a.clone()).unwrap_or_default();
    shared.emit(Event::Accounts(accounts.clone()));
    for a in &accounts {
        shared.emit_mailboxes(&a.id);
        workers.insert(a.id.clone(), spawn_worker(shared.clone(), a.id.clone()));
    }
    shared.emit_pending();
    while let Some(cmd) = rx.recv().await {
        if let Err(e) = handle(&shared, &mut workers, cmd).await {
            shared.emit(Event::Error(format!("{e:#}")));
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

async fn handle(shared: &Arc<Shared>, workers: &mut HashMap<String, Worker>, cmd: Command) -> Result<()> {
    match cmd {
        Command::List { req, view, query, conversations, limit } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                let me = shared.me();
                let q = Query::parse(&query);
                let items = shared.reader.lock().map_err(|_| anyhow!("store poisoned")).and_then(|st| st.list(&view, &q, conversations, limit, &me));
                match items {
                    Ok(items) => shared.emit(Event::List { req, items }),
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
                poke(workers, &account, Job::Bodies { mailbox, uids, reply: Some((req, id, conversations)) });
            }
        }
        Command::SetFlags { ids, add, remove, thread } => {
            let mut touched = HashSet::new();
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
            for a in touched {
                shared.emit_mailboxes(&a);
                shared.emit(Event::Changed { account: a.clone() });
                poke(workers, &a, Job::Sync);
            }
            shared.emit_pending();
        }
        Command::Move { ids, to, thread, undo } => {
            let ids = expand(shared, ids, thread, true)?;
            let window = shared.setting_u32("behavior.undoSeconds");
            // A second of grace on top of the window, so the toast's Undo button (shown for
            // exactly the window) never outlives the hold, whatever the clocks' rounding.
            let hold = undo.filter(|_| window > 0).map(|key| (now() + window as i64 + 1, key));
            let (touched, mut problems) = shared.with_writer(|st| st.transaction(|st| move_local(st, &ids, &to, hold)))?;
            if let Some((_, key)) = hold
                && !touched.is_empty()
                && let Ok(mut keys) = shared.undo_keys.lock()
            {
                // Only the last few can still be undone; keep the set small.
                if keys.len() >= 256 {
                    keys.clear();
                }
                keys.insert(key);
            }
            problems.dedup();
            for p in problems {
                shared.emit(Event::Error(p));
            }
            for a in touched {
                shared.emit_mailboxes(&a);
                shared.emit(Event::Changed { account: a.clone() });
                // Wakes the worker: it replays at once, or re-plans its wait around the hold.
                poke(workers, &a, Job::Sync);
            }
            shared.emit_pending();
        }
        Command::UndoMove { undo } => undo_move(shared, undo)?,
        Command::Send { draft } => {
            let account = shared.account(&draft.account).ok_or_else(|| anyhow!("Choose an account to send from"))?;
            let built = smtp::build(&account, &draft)?;
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
            let id = shared.with_writer(|st| st.enqueue_at(&account.id, &op, now() + undo as i64))?;
            shared.emit(Event::Sent { op: id, undo_secs: undo });
            shared.emit_pending();
            poke(workers, &account.id, Job::Sync);
        }
        Command::UndoSend { op } => {
            let cancelled = shared.with_writer(|st| st.cancel_send(op))?;
            shared.emit(Event::Toast(if cancelled { "Sending undone".into() } else { "Too late — it was already sent".into() }));
            shared.emit_pending();
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

/// [`Command::UndoMove`]: takes back what's still queued of a move and tells the UI how it went.
fn undo_move(shared: &Shared, undo: i64) -> Result<()> {
    let touched = shared.with_writer(|st| st.undo_batch(undo))?;
    let queued = shared.undo_keys.lock().is_ok_and(|mut k| k.remove(&undo));
    if touched.is_empty() {
        // Nothing was held: it already replayed, or the move had nothing to do (say,
        // archiving in All Mail). The UI already put the rows back; a reload shows
        // where the messages really are.
        if queued {
            shared.emit(Event::Toast("Too late to undo — it already reached the server".into()));
        }
        shared.emit(Event::Changed { account: String::new() });
    }
    for a in touched {
        shared.emit_mailboxes(&a);
        shared.emit(Event::Changed { account: a });
    }
    shared.emit_pending();
    Ok(())
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
fn move_local(st: &Store, ids: &[MessageId], to: &MoveTarget, hold: Option<(i64, i64)>) -> Result<(HashSet<String>, Vec<String>)> {
    let mut touched = HashSet::new();
    let mut problems = Vec::new();
    for id in ids {
        let Some((account, mailbox, uid)) = st.location(*id)? else { continue };
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
                Some((not_before, key)) => st.enqueue_held(&account, &op, not_before, key)?,
                None => st.enqueue(&account, &op)?,
            };
            st.hide_local(*id, op_id)?;
        }
        if unread {
            st.bump_unread(&account, &mailbox, -1)?;
        }
        touched.insert(account);
    }
    Ok((touched, problems))
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
        Job::Bodies { mailbox, uids, reply } => {
            imap::select(session, mailbox).await?;
            imap::fetch_bodies(session, &shared.writer, id, mailbox, uids).await?;
            if let Some((req, mid, conv)) = reply {
                let messages = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?.thread(*mid, *conv)?;
                shared.emit(Event::Thread { req: *req, id: *mid, messages });
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
    let (has_move, has_idle) = imap::capabilities(&mut session).await?;
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
            if let Job::Bodies { mailbox, uids, reply } = job {
                imap::select(&mut session, &mailbox).await?;
                imap::fetch_bodies(&mut session, &shared.writer, &id, &mailbox, &uids).await?;
                if let Some((req, mid, conv)) = reply {
                    let messages = shared.reader.lock().map_err(|_| anyhow!("store poisoned"))?.thread(mid, conv)?;
                    shared.emit(Event::Thread { req, id: mid, messages });
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
        let ran_ops = replay_ops(shared, account, &mut session, has_move).await?;

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
        if has_idle && shared.setting_bool("sync.useIdle") && inbox.is_some() {
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

async fn replay_ops(shared: &Arc<Shared>, account: &Account, session: &mut Session, has_move: bool) -> Result<bool> {
    replay_with(shared, account, &mut ImapRunner { shared, account, session, has_move }).await
}

/// Carries out one queued op on the server. A seam so replay can be tested without a server.
trait OpRunner {
    fn run(&mut self, op: &Op) -> impl std::future::Future<Output = Result<()>> + Send;
}

struct ImapRunner<'a> {
    shared: &'a Arc<Shared>,
    account: &'a Account,
    session: &'a mut Session,
    has_move: bool,
}

impl OpRunner for ImapRunner<'_> {
    async fn run(&mut self, op: &Op) -> Result<()> {
        let (shared, account, session) = (self.shared, self.account, &mut *self.session);
        match op {
            Op::SetFlags { mailbox, uid, add, remove } => imap::store_flags(session, mailbox, *uid, *add, *remove).await,
            Op::Move { mailbox, uid, dest } => imap::move_message(session, self.has_move, mailbox, *uid, dest).await,
            Op::Expunge { mailbox, uid } => imap::expunge(session, mailbox, *uid, true).await,
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
    let ops = shared.with_writer(|st| st.ops(&account.id, now()))?;
    let mut any = false;
    for q in ops {
        let r = runner.run(&q.op).await;
        match r {
            Ok(()) => {
                shared.with_writer(|st| st.op_done(q.id))?;
                any = true;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let connection_lost = msg.contains("timed out") || msg.contains("connection") || msg.contains("Connection") || msg.contains("broken pipe");
                // A message the server rejects for good (gone, no such mailbox) is dropped
                // after a few tries; network trouble keeps it queued.
                if q.attempts >= 5 && !connection_lost && !matches!(q.op, Op::Send { .. }) {
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
    if any {
        shared.emit_pending();
    }
    Ok(any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(st: &Store, mailbox: &str, uid: u32, mid: &str, refs: &str, fl: u32) -> MessageId {
        let raw = format!("From: Ana <ana@x.com>\r\nSubject: Plans\r\nMessage-ID: {mid}\r\nReferences: {refs}\r\nDate: Tue, 3 Mar 2026 0{uid}:00:00 +0000\r\n\r\nhi\r\n");
        let p = crate::mime::parse(raw.as_bytes(), false).unwrap();
        st.upsert_header("a", mailbox, uid, fl, 10, 0, &p).unwrap()
    }

    fn setup() -> Store {
        let st = Store::memory();
        for (name, role) in [("INBOX", Role::Inbox), ("Archive", Role::Archive), ("Trash", Role::Trash), ("Junk", Role::Junk)] {
            st.upsert_mailbox("a", name, name, role, "/", true).unwrap();
        }
        st
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
        let (touched, problems) = move_local(&st, &ids, &MoveTarget::Archive, Some((1_000, 42))).unwrap();
        assert_eq!(touched, HashSet::from(["a".to_owned()]));
        assert!(problems.is_empty());
        assert!(visible(&st, "INBOX").is_empty());
        assert_eq!(st.mailboxes("a").unwrap()[0].unread, 0);
        assert!(st.ops("a", 999).unwrap().is_empty(), "held for the window");
        assert_eq!(st.ops("a", 1_000).unwrap().len(), 3);

        assert_eq!(st.undo_batch(42).unwrap(), vec!["a".to_owned()]);
        let mut back = visible(&st, "INBOX");
        back.sort();
        assert_eq!(back, vec![first, reply, other], "every message back, same ids");
        assert_eq!(st.mailboxes("a").unwrap()[0].unread, 2);
        assert_eq!(st.pending_count().unwrap(), 0, "nothing reaches the server");
        let thread = st.thread(reply, true).unwrap();
        assert_eq!(thread.len(), 2, "the conversation opens whole again");
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
        assert!(st.undo_batch(3).unwrap().is_empty());
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

    #[test]
    fn archiving_in_the_archive_queues_nothing() {
        let st = setup();
        let a = message(&st, "Archive", 1, "<1@x>", "", 0);
        let (touched, _) = move_local(&st, &[a], &MoveTarget::Archive, Some((1_000, 5))).unwrap();
        assert!(touched.is_empty());
        assert_eq!(visible(&st, "Archive"), vec![a]);
    }
}
