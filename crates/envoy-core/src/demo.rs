//! Demo mode's mail server: an IMAP server in this process, which the engine reaches over an
//! in-memory pipe ([`crate::imap::Stream::Local`]) instead of a network. `--demo` runs the real
//! engine against it: sync, the op queue's moves, flags, labels, drafts and sends, snoozes and
//! their wake, all as they'd go with a real server, and nothing ever leaves the computer
//! ([`crate::net`]). Sends are handed to [`Server::deliver`], which files them as Gmail would
//! (and delivers mail to the demo's own addresses).
//!
//! It speaks the part of IMAP4rev1 Envoy uses (with IDLE, MOVE and UIDPLUS), over mail kept in
//! memory. A Gmail account's folders behave as Gmail's labels do: a message is one message in
//! every folder it's in (its flags are shared), `[Gmail]/All Mail` (`\All`) holds everything but
//! Spam and Trash, `[Gmail]/Starred` (`\Flagged`) whatever is starred, moving to Trash or Spam
//! takes a message out of its other folders, and moving to All Mail archives it.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::accounts::Account;
use crate::imap;
use crate::model::flags;

/// The largest message the demo's SMTP side takes, as Gmail's says.
pub const SEND_LIMIT: u64 = 35_882_577;

/// A sample message, to make into one as it would arrive ([`Mail::rfc822`]).
#[derive(Clone, Debug, Default)]
pub struct Mail {
    /// `(name, address)`.
    pub from: (String, String),
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub subject: String,
    /// Unix seconds.
    pub date: i64,
    /// Without the angle brackets.
    pub message_id: String,
    /// The conversation's earlier Message-IDs, oldest first (the last is the one replied to).
    pub references: Vec<String>,
    /// Any other header fields (`List-Unsubscribe`, `Auto-Submitted`…).
    pub headers: Vec<(String, String)>,
    pub text: Option<String>,
    pub html: Option<String>,
    /// `(file name, type, size)`: a file of that size, made up.
    pub attachment: Option<(String, String, usize)>,
}

impl Mail {
    /// The message as a server would hand it over: MIME, every header encoded as it should be.
    pub fn rfc822(&self) -> Vec<u8> {
        use base64::Engine as _;
        let b64 = |bytes: &[u8]| -> String {
            let line = base64::engine::general_purpose::STANDARD.encode(bytes);
            line.as_bytes().chunks(76).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("\r\n")
        };
        let word = |s: &str| if s.is_ascii() { s.to_owned() } else { format!("=?utf-8?b?{}?=", base64::engine::general_purpose::STANDARD.encode(s)) };
        let people = |list: &[(String, String)]| list.iter().map(|(n, e)| if n.is_empty() { e.clone() } else { format!("\"{}\" <{e}>", word(n)) }).collect::<Vec<_>>().join(", ");
        let date = chrono::DateTime::from_timestamp(self.date, 0).unwrap_or_default().format("%a, %d %b %Y %H:%M:%S +0000");
        let mut h = format!("From: {}\r\nTo: {}\r\n", people(std::slice::from_ref(&self.from)), people(&self.to));
        if !self.cc.is_empty() {
            h.push_str(&format!("Cc: {}\r\n", people(&self.cc)));
        }
        h.push_str(&format!("Subject: {}\r\nDate: {date}\r\nMessage-ID: <{}>\r\n", word(&self.subject), self.message_id));
        if let Some(last) = self.references.last() {
            let refs: Vec<String> = self.references.iter().map(|r| format!("<{r}>")).collect();
            h.push_str(&format!("In-Reply-To: <{last}>\r\nReferences: {}\r\n", refs.join(" ")));
        }
        for (name, value) in &self.headers {
            h.push_str(&format!("{name}: {value}\r\n"));
        }
        h.push_str("MIME-Version: 1.0\r\n");
        let part = |mime: &str, body: &str| format!("Content-Type: {mime}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", b64(body.as_bytes()));
        let body = match (&self.text, &self.html) {
            (Some(t), Some(html)) => format!("Content-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n--alt\r\n{}--alt\r\n{}--alt--\r\n", part("text/plain", t), part("text/html", html)),
            (None, Some(html)) => part("text/html", html),
            (t, None) => part("text/plain", t.as_deref().unwrap_or_default()),
        };
        let Some((name, mime, size)) = &self.attachment else {
            return format!("{h}{body}").into_bytes();
        };
        // Made-up contents, of the size it says.
        let mut file = format!("%PDF-1.4\n% {name}: a sample file of Envoy's demo mode\n").into_bytes();
        file.resize((*size).max(file.len()), b' ');
        format!(
            "{h}Content-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n--mix\r\n{body}--mix\r\nContent-Type: {mime}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--mix--\r\n",
            b64(&file)
        )
        .into_bytes()
    }
}

/// The in-process server: every demo account's mail.
pub struct Server {
    data: Mutex<Vec<Mailstore>>,
    /// Bumped on every change, so a connection in IDLE can tell its client of new mail.
    changes: tokio::sync::watch::Sender<u64>,
}

/// One account's folders and messages.
struct Mailstore {
    account: Account,
    gmail: bool,
    folders: Vec<Folder>,
    /// By index; a message deleted for good leaves `None`.
    messages: Vec<Option<Message>>,
    next_validity: u32,
}

struct Folder {
    name: String,
    /// Its special-use attribute (`\Sent`, `\All`…), or empty.
    special: &'static str,
    validity: u32,
    next_uid: u32,
    /// In UID order.
    members: Vec<Member>,
}

struct Member {
    uid: u32,
    message: usize,
    /// Flagged `\Deleted` here, not expunged yet.
    deleted: bool,
}

struct Message {
    raw: Arc<[u8]>,
    /// INTERNALDATE, Unix seconds.
    date: i64,
    /// [`flags`] bits, but `DELETED` (a folder's own).
    flags: u32,
    message_id: String,
}

/// What one connection has selected: the folder, and its UIDs as it was told of them (for
/// sequence numbers, and for news in IDLE).
struct Selected {
    folder: String,
    uids: Vec<u32>,
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}

impl Server {
    pub fn new() -> Server {
        Server { data: Mutex::new(Vec::new()), changes: tokio::sync::watch::Sender::new(0) }
    }

    /// Adds an account with these folders, `(name, special-use attribute or "")`, empty.
    pub fn add_account(&self, account: Account, folders: &[(&str, &'static str)]) {
        let gmail = account.is_gmail();
        let mut store = Mailstore { account, gmail, folders: Vec::new(), messages: Vec::new(), next_validity: 1 };
        for (name, special) in folders {
            store.create(name, special);
        }
        self.lock().push(store);
    }

    /// The accounts, as added.
    pub fn accounts(&self) -> Vec<Account> {
        self.lock().iter().map(|s| s.account.clone()).collect()
    }

    /// Puts a message in `folders` of `account` (one message in each, on Gmail; a copy in each
    /// elsewhere), with these [`flags`] and INTERNALDATE. Folders it doesn't have are made.
    pub fn add_mail(&self, account: &str, folders: &[&str], raw: Vec<u8>, flags: u32, date: i64) {
        let mut data = self.lock();
        let Some(store) = data.iter_mut().find(|s| s.account.id == account) else { return };
        let raw: Arc<[u8]> = raw.into();
        let mut first = None;
        for name in folders {
            let f = store.folder(name).unwrap_or_else(|| store.create(name, ""));
            match first {
                Some(m) if store.gmail => store.add(f, m),
                _ => {
                    let m = store.message(raw.clone(), flags, date);
                    first = Some(m);
                    store.add(f, m)
                }
            };
        }
        store.settle();
        drop(data);
        self.changed();
    }

    /// Hands a sent message to the "SMTP server": Gmail files it in Sent itself, and a copy goes
    /// to the inbox of each recipient who's one of the demo's accounts. Nothing else happens to
    /// it: it goes nowhere.
    pub fn deliver(&self, from: &Account, raw: &[u8], to: &[String]) {
        let now = chrono::Utc::now().timestamp();
        let mut data = self.lock();
        for store in data.iter_mut() {
            if store.account.id == from.id
                && store.gmail
                && let Some(sent) = store.special("\\Sent")
            {
                store.append(sent, raw.into(), flags::SEEN, now);
            }
            if to.iter().any(|t| t.eq_ignore_ascii_case(&store.account.email))
                && let Some(inbox) = store.folder("INBOX")
            {
                store.append(inbox, raw.into(), 0, now);
            }
        }
        drop(data);
        self.changed();
    }

    /// What's in a folder: each message's flags (`\Deleted` included) and Message-ID, in UID
    /// order. For tests, and anyone curious.
    pub fn contents(&self, account: &str, folder: &str) -> Vec<(u32, String)> {
        let data = self.lock();
        let Some(store) = data.iter().find(|s| s.account.id == account) else { return Vec::new() };
        let Some(f) = store.folder(folder) else { return Vec::new() };
        store.folders[f].members.iter().map(|m| (store.flags_of(m), store.messages[m.message].as_ref().map(|x| x.message_id.clone()).unwrap_or_default())).collect()
    }

    /// The account's folders' names.
    pub fn folders(&self, account: &str) -> Vec<String> {
        self.lock().iter().find(|s| s.account.id == account).map(|s| s.folders.iter().map(|f| f.name.clone()).collect()).unwrap_or_default()
    }

    /// A connection to `account`'s mail, logged in: what [`imap::connect`] gives for a real one.
    /// Needs a tokio runtime (the connection's server side runs on it).
    pub async fn connect(self: &Arc<Self>, account: &Account) -> Result<imap::Session> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(self.clone().serve(account.id.clone(), server));
        let mut client = async_imap::Client::new(imap::Stream::Local(client));
        client.read_response().await.ok_or_else(|| anyhow!("the demo server closed the connection"))??;
        let session = client.login(account.login(), "demo").await.map_err(|(e, _)| anyhow!(e))?;
        Ok(imap::Connection::new(session))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Mailstore>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// One connection: reads commands until the client logs out or goes.
    async fn serve(self: Arc<Self>, account: String, io: tokio::io::DuplexStream) {
        let (r, mut w) = tokio::io::split(io);
        let mut r = BufReader::new(r);
        let mut changes = self.changes.subscribe();
        let mut selected: Option<Selected> = None;
        if w.write_all(b"* OK [CAPABILITY IMAP4rev1 IDLE MOVE UIDPLUS] Envoy demo, in this process\r\n").await.is_err() {
            return;
        }
        let mut line = Vec::new();
        loop {
            line.clear();
            if !matches!(r.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                return;
            }
            let text = String::from_utf8_lossy(&line).trim_end().to_owned();
            let (tag, rest) = text.split_once(' ').unwrap_or((text.as_str(), ""));
            // A literal ends the line: asked for, read, and the line's end after it.
            let literal = match rest.strip_suffix('}').and_then(|c| c.rsplit_once('{')).and_then(|(_, n)| n.parse::<usize>().ok()) {
                Some(n) => {
                    let mut bytes = vec![0; n];
                    let mut end = Vec::new();
                    if w.write_all(b"+ go ahead\r\n").await.is_err() || r.read_exact(&mut bytes).await.is_err() || r.read_until(b'\n', &mut end).await.is_err() {
                        return;
                    }
                    Some(bytes)
                }
                None => None,
            };
            let (verb, args) = rest.split_once(' ').unwrap_or((rest, ""));
            let verb = verb.to_ascii_uppercase();
            let (uid, verb, args) = match verb.as_str() {
                "UID" => {
                    let (v, a) = args.split_once(' ').unwrap_or((args, ""));
                    (true, v.to_ascii_uppercase(), a)
                }
                _ => (false, verb, args),
            };
            match verb.as_str() {
                "LOGOUT" => {
                    let _ = w.write_all(format!("* BYE so long\r\n{tag} OK LOGOUT done\r\n").as_bytes()).await;
                    return;
                }
                "IDLE" => {
                    let _ = changes.borrow_and_update();
                    if w.write_all(b"+ idling\r\n").await.is_err() {
                        return;
                    }
                    let mut done = Vec::new();
                    loop {
                        tokio::select! {
                            read = r.read_until(b'\n', &mut done) => {
                                if !matches!(read, Ok(n) if n > 0) {
                                    return;
                                }
                                break;
                            }
                            Ok(()) = changes.changed() => {
                                let news = self.news(&account, &mut selected);
                                if !news.is_empty() && w.write_all(&news).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    if w.write_all(format!("{tag} OK IDLE done\r\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                _ => {
                    let (reply, changed) = self.command(&account, &mut selected, tag, &verb, uid, args, literal);
                    if changed {
                        self.changed();
                        let _ = changes.borrow_and_update();
                    }
                    if w.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// What a connection in IDLE is told: mail that came to (or went from) its folder.
    fn news(&self, account: &str, selected: &mut Option<Selected>) -> Vec<u8> {
        let data = self.lock();
        let (Some(store), Some(sel)) = (data.iter().find(|s| s.account.id == account), selected.as_mut()) else { return Vec::new() };
        let Some(f) = store.folder(&sel.folder) else { return Vec::new() };
        let now: Vec<u32> = store.folders[f].members.iter().map(|m| m.uid).collect();
        let mut out = String::new();
        for (i, uid) in sel.uids.iter().enumerate().rev() {
            if !now.contains(uid) {
                out.push_str(&format!("* {} EXPUNGE\r\n", i + 1));
            }
        }
        sel.uids.retain(|u| now.contains(u));
        if now.len() != sel.uids.len() {
            out.push_str(&format!("* {} EXISTS\r\n", now.len()));
            sel.uids = now;
        }
        out.into_bytes()
    }

    /// Runs one command: its answer, tagged line and all, and whether it changed any mail.
    #[allow(clippy::too_many_arguments)]
    fn command(&self, account: &str, selected: &mut Option<Selected>, tag: &str, verb: &str, uid: bool, args: &str, literal: Option<Vec<u8>>) -> (Vec<u8>, bool) {
        let mut data = self.lock();
        let Some(store) = data.iter_mut().find(|s| s.account.id == account) else { return (format!("{tag} NO no such account\r\n").into_bytes(), false) };
        let a = words(args);
        let arg = |i: usize| a.get(i).map(String::as_str).unwrap_or("");
        let mut out = Vec::new();
        let ok = |out: &mut Vec<u8>, what: &str| out.extend_from_slice(format!("{tag} OK {what}\r\n").as_bytes());
        let no = |out: &mut Vec<u8>, what: &str| out.extend_from_slice(format!("{tag} NO {what}\r\n").as_bytes());
        let mut changed = false;
        match verb {
            "LOGIN" | "NOOP" | "CHECK" | "SUBSCRIBE" | "UNSUBSCRIBE" | "ENABLE" | "ID" => ok(&mut out, "done"),
            "CAPABILITY" => {
                out.extend_from_slice(b"* CAPABILITY IMAP4rev1 IDLE MOVE UIDPLUS\r\n");
                ok(&mut out, "done");
            }
            "LIST" => {
                let pattern = arg(1);
                for f in store.folders.iter().filter(|f| pattern == "*" || f.name == pattern || (pattern == "%" && !f.name.contains('/'))) {
                    out.extend_from_slice(format!("* LIST ({}) \"/\" {}\r\n", f.special, quoted(&f.name)).as_bytes());
                }
                ok(&mut out, "LIST done");
            }
            "SELECT" | "EXAMINE" => match store.folder(arg(0)) {
                Some(f) => {
                    let folder = &store.folders[f];
                    out.extend_from_slice(
                        format!(
                            "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $new)\r\n* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $new \\*)] ok\r\n* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {}] ok\r\n* OK [UIDNEXT {}] ok\r\n",
                            folder.members.len(),
                            folder.validity,
                            folder.next_uid
                        )
                        .as_bytes(),
                    );
                    *selected = Some(Selected { folder: folder.name.clone(), uids: folder.members.iter().map(|m| m.uid).collect() });
                    ok(&mut out, "[READ-WRITE] SELECT done");
                }
                None => {
                    *selected = None;
                    no(&mut out, "[NONEXISTENT] no such mailbox");
                }
            },
            "STATUS" => match store.folder(arg(0)) {
                Some(f) => {
                    let folder = &store.folders[f];
                    let unseen = folder.members.iter().filter(|m| store.flags_of(m) & flags::SEEN == 0).count();
                    out.extend_from_slice(
                        format!("* STATUS {} (MESSAGES {} UNSEEN {unseen} UIDNEXT {} UIDVALIDITY {})\r\n", quoted(&folder.name), folder.members.len(), folder.next_uid, folder.validity).as_bytes(),
                    );
                    ok(&mut out, "STATUS done");
                }
                None => no(&mut out, "[NONEXISTENT] no such mailbox"),
            },
            "CREATE" => {
                if store.folder(arg(0)).is_some() {
                    no(&mut out, "[ALREADYEXISTS] it's there");
                } else {
                    store.create(arg(0), "");
                    changed = true;
                    ok(&mut out, "CREATE done");
                }
            }
            "DELETE" => match store.folder(arg(0)) {
                Some(f) => {
                    store.folders.remove(f);
                    store.settle();
                    changed = true;
                    ok(&mut out, "DELETE done");
                }
                None => no(&mut out, "[NONEXISTENT] no such mailbox"),
            },
            "APPEND" => match (store.folder(arg(0)), literal) {
                (Some(f), Some(raw)) => {
                    let fl = a.get(1).filter(|f| f.starts_with('(')).map_or(0, |f| flag_bits(f));
                    let now = chrono::Utc::now().timestamp();
                    let uid = store.append(f, raw.into(), fl & !flags::DELETED, now);
                    changed = true;
                    ok(&mut out, &format!("[APPENDUID {} {uid}] APPEND done", store.folders[f].validity));
                }
                (None, _) => no(&mut out, "[TRYCREATE] no such mailbox"),
                (_, None) => out.extend_from_slice(format!("{tag} BAD a message to append, please\r\n").as_bytes()),
            },
            "CLOSE" | "UNSELECT" => {
                *selected = None;
                ok(&mut out, "done");
            }
            "FETCH" | "STORE" | "SEARCH" | "COPY" | "MOVE" | "EXPUNGE" => {
                let Some(f) = selected.as_ref().and_then(|s| store.folder(&s.folder)) else {
                    out.extend_from_slice(format!("{tag} BAD select a mailbox first\r\n").as_bytes());
                    return (out, false);
                };
                let sel = selected.as_mut().expect("selected above");
                // The members a set names: by UID, or by sequence number in what this
                // connection was told of the folder.
                let targets = |store: &Mailstore, set: &str| -> Vec<u32> {
                    let members = &store.folders[f].members;
                    if uid {
                        let max = members.last().map_or(0, |m| m.uid);
                        members.iter().map(|m| m.uid).filter(|u| in_set(set, *u, max)).collect()
                    } else {
                        let max = sel.uids.len() as u32;
                        sel.uids.iter().enumerate().filter(|(i, _)| in_set(set, *i as u32 + 1, max)).map(|(_, u)| *u).filter(|u| members.iter().any(|m| m.uid == *u)).collect()
                    }
                };
                match verb {
                    "FETCH" => {
                        let uids = targets(store, arg(0));
                        changed = store.fetch(f, &uids, arg(1), uid, &mut out);
                        ok(&mut out, "FETCH done");
                    }
                    "STORE" => {
                        let uids = targets(store, arg(0));
                        let mode = arg(1).to_ascii_uppercase();
                        let bits = flag_bits(arg(2));
                        for u in &uids {
                            store.store(f, *u, &mode, bits);
                            if !mode.contains(".SILENT")
                                && let Some((seq, m)) = store.folders[f].members.iter().enumerate().find(|(_, m)| m.uid == *u)
                            {
                                out.extend_from_slice(format!("* {} FETCH (UID {u} FLAGS ({}))\r\n", seq + 1, imap::flag_names(store.flags_of(m))).as_bytes());
                            }
                        }
                        store.settle();
                        changed = !uids.is_empty();
                        ok(&mut out, "STORE done");
                    }
                    "SEARCH" => {
                        let found: Vec<String> = store.folders[f]
                            .members
                            .iter()
                            .enumerate()
                            .filter(|(_, m)| store.matches(m, &a))
                            .map(|(i, m)| if uid { m.uid } else { i as u32 + 1 }.to_string())
                            .collect();
                        out.extend_from_slice(format!("* SEARCH {}\r\n", found.join(" ")).trim_end().as_bytes());
                        out.extend_from_slice(b"\r\n");
                        ok(&mut out, "SEARCH done");
                    }
                    "COPY" | "MOVE" => match store.folder(arg(1)) {
                        Some(dest) => {
                            let uids = targets(store, arg(0));
                            let before: Vec<u32> = store.folders[f].members.iter().map(|m| m.uid).collect();
                            for u in &uids {
                                if verb == "MOVE" { store.move_one(f, *u, dest) } else { store.copy_one(f, *u, dest) }
                            }
                            store.settle();
                            expunged(&before, &store.folders[f], sel, &mut out);
                            changed = !uids.is_empty();
                            ok(&mut out, &format!("{verb} done"));
                        }
                        None => no(&mut out, "[TRYCREATE] no such mailbox"),
                    },
                    _ => {
                        // EXPUNGE, or UID EXPUNGE of a set: messages flagged \Deleted go.
                        let uids = if uid { targets(store, arg(0)) } else { store.folders[f].members.iter().map(|m| m.uid).collect() };
                        let before: Vec<u32> = store.folders[f].members.iter().map(|m| m.uid).collect();
                        store.expunge(f, &uids);
                        store.settle();
                        changed = expunged(&before, &store.folders[f], sel, &mut out);
                        ok(&mut out, "EXPUNGE done");
                    }
                }
            }
            _ => out.extend_from_slice(format!("{tag} BAD not a command this server knows\r\n").as_bytes()),
        }
        (out, changed)
    }
}

/// Puts everything Envoy writes from here on (settings, the cache, drafts kept at close, exports)
/// in a folder of this run's own ([`crate::paths::use_home`]), for demo mode, which must neither
/// read nor change the user's; [`clean_up`] takes it away. Folders left by demos that didn't get
/// to close are taken away here. False if something asked where a file goes first.
pub fn isolate() -> bool {
    let temp = std::env::temp_dir();
    let home = home_of(std::process::id());
    let _ = std::fs::remove_dir_all(&home);
    let isolated = crate::paths::use_home(home);
    let day = std::time::Duration::from_secs(24 * 3600);
    for e in std::fs::read_dir(&temp).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("envoy-demo-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > day));
        if pid != std::process::id() && (old || !running(pid)) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    isolated
}

/// Takes away this run's demo folder ([`isolate`]), as demo mode closes.
pub fn clean_up() {
    let _ = std::fs::remove_dir_all(home_of(std::process::id()));
}

fn home_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("envoy-demo-{pid}"))
}

/// Whether process `pid` is still running, where that's cheap to tell (elsewhere, it's taken to
/// be, and its folder waits a day).
fn running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 asks without sending anything: an error says there's no such process.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Tells of the messages that left `folder` (UIDs `before` it changed) as EXPUNGE responses,
/// highest first, and forgets them from what the connection was told. True when any did.
fn expunged(before: &[u32], folder: &Folder, sel: &mut Selected, out: &mut Vec<u8>) -> bool {
    let gone: Vec<u32> = before.iter().copied().filter(|u| !folder.members.iter().any(|m| m.uid == *u)).collect();
    for (i, u) in sel.uids.iter().enumerate().rev() {
        if gone.contains(u) {
            out.extend_from_slice(format!("* {} EXPUNGE\r\n", i + 1).as_bytes());
        }
    }
    sel.uids.retain(|u| !gone.contains(u));
    !gone.is_empty()
}

impl Mailstore {
    fn folder(&self, name: &str) -> Option<usize> {
        self.folders.iter().position(|f| f.name == name || (f.name == "INBOX" && name.eq_ignore_ascii_case("INBOX")))
    }

    fn special(&self, special: &str) -> Option<usize> {
        self.folders.iter().position(|f| f.special == special)
    }

    fn create(&mut self, name: &str, special: &'static str) -> usize {
        self.folders.push(Folder { name: name.to_owned(), special, validity: self.next_validity, next_uid: 1, members: Vec::new() });
        self.next_validity += 1;
        self.folders.len() - 1
    }

    fn message(&mut self, raw: Arc<[u8]>, flags: u32, date: i64) -> usize {
        let message_id = crate::mime::parse(header(&raw), true).map(|p| p.message_id).unwrap_or_default();
        self.messages.push(Some(Message { raw, date, flags: flags & !flags::DELETED, message_id }));
        self.messages.len() - 1
    }

    /// Puts message `m` in folder `f` (unless it's there), under the folder's next UID.
    fn add(&mut self, f: usize, m: usize) -> u32 {
        let folder = &mut self.folders[f];
        if let Some(there) = folder.members.iter().find(|x| x.message == m) {
            return there.uid;
        }
        let uid = folder.next_uid;
        folder.next_uid += 1;
        folder.members.push(Member { uid, message: m, deleted: false });
        uid
    }

    /// A new message in folder `f`: its UID.
    fn append(&mut self, f: usize, raw: Arc<[u8]>, flags: u32, date: i64) -> u32 {
        let m = self.message(raw, flags, date);
        let uid = self.add(f, m);
        self.settle();
        uid
    }

    fn flags_of(&self, m: &Member) -> u32 {
        self.messages[m.message].as_ref().map_or(0, |x| x.flags) | if m.deleted { flags::DELETED } else { 0 }
    }

    /// Gmail's folders that are views: All Mail (everything but Spam and Trash) and Starred.
    fn is_view(&self, f: usize) -> bool {
        self.gmail && matches!(self.folders[f].special, "\\All" | "\\Flagged")
    }

    fn is_bin(&self, f: usize) -> bool {
        matches!(self.folders[f].special, "\\Trash" | "\\Junk")
    }

    /// Brings the folders that are views up to date with the messages, and forgets messages in
    /// no folder at all (but on Gmail, where they're in All Mail).
    fn settle(&mut self) {
        let mut in_folder = vec![false; self.messages.len()];
        let mut in_bin = vec![false; self.messages.len()];
        for (f, folder) in self.folders.iter().enumerate() {
            if self.gmail && matches!(folder.special, "\\All" | "\\Flagged") {
                continue;
            }
            for m in &folder.members {
                in_folder[m.message] = true;
                in_bin[m.message] |= matches!(self.folders[f].special, "\\Trash" | "\\Junk");
            }
        }
        if !self.gmail {
            for (i, m) in self.messages.iter_mut().enumerate() {
                if !in_folder[i] {
                    *m = None;
                }
            }
            return;
        }
        for f in 0..self.folders.len() {
            let starred = match self.folders[f].special {
                "\\All" => false,
                "\\Flagged" => true,
                _ => continue,
            };
            let wanted = |i: usize, messages: &[Option<Message>]| messages[i].as_ref().is_some_and(|m| !in_bin[i] && (!starred || m.flags & flags::FLAGGED != 0));
            let messages = &self.messages;
            self.folders[f].members.retain(|m| wanted(m.message, messages));
            for i in 0..self.messages.len() {
                if wanted(i, &self.messages) {
                    self.add(f, i);
                }
            }
        }
    }

    fn member(&self, f: usize, uid: u32) -> Option<usize> {
        self.folders[f].members.iter().find(|m| m.uid == uid).map(|m| m.message)
    }

    fn take_out(&mut self, f: usize, m: usize) {
        self.folders[f].members.retain(|x| x.message != m);
    }

    fn move_one(&mut self, from: usize, uid: u32, to: usize) {
        let Some(m) = self.member(from, uid) else { return };
        if from == to {
            return;
        }
        if !self.gmail {
            self.take_out(from, m);
            self.add(to, m);
            return;
        }
        if self.is_bin(to) {
            // Trash or Spam: out of every other folder.
            for f in 0..self.folders.len() {
                if !self.is_view(f) {
                    self.take_out(f, m);
                }
            }
            self.add(to, m);
            return;
        }
        if !self.is_view(from) {
            self.take_out(from, m);
        }
        match self.folders[to].special {
            // Archived: in All Mail as it is.
            "\\All" => {}
            "\\Flagged" => self.flag(m, flags::FLAGGED, true),
            _ => {
                self.add(to, m);
            }
        }
    }

    fn copy_one(&mut self, from: usize, uid: u32, to: usize) {
        let Some(m) = self.member(from, uid) else { return };
        if self.gmail {
            if self.is_bin(to) {
                return self.move_one(from, uid, to);
            }
            if !self.is_view(to) {
                self.add(to, m);
            }
            return;
        }
        let Some(x) = self.messages[m].as_ref() else { return };
        let (raw, fl, date) = (x.raw.clone(), x.flags, x.date);
        let copy = self.message(raw, fl, date);
        self.add(to, copy);
    }

    fn flag(&mut self, m: usize, bits: u32, on: bool) {
        if let Some(x) = self.messages[m].as_mut() {
            if on { x.flags |= bits } else { x.flags &= !bits }
        }
    }

    fn store(&mut self, f: usize, uid: u32, mode: &str, bits: u32) {
        let Some(member) = self.folders[f].members.iter_mut().find(|m| m.uid == uid) else { return };
        let m = member.message;
        let shared = bits & !flags::DELETED;
        match mode.chars().next() {
            Some('+') => {
                member.deleted |= bits & flags::DELETED != 0;
                self.flag(m, shared, true);
            }
            Some('-') => {
                member.deleted &= bits & flags::DELETED == 0;
                self.flag(m, shared, false);
            }
            _ => {
                member.deleted = bits & flags::DELETED != 0;
                if let Some(x) = self.messages[m].as_mut() {
                    x.flags = shared;
                }
            }
        }
    }

    /// Takes the messages of `uids` flagged `\Deleted` out of folder `f`: on Gmail, out of
    /// existence from Trash, Spam, Drafts or All Mail, and only out of a label elsewhere.
    fn expunge(&mut self, f: usize, uids: &[u32]) {
        let gone: Vec<usize> = self.folders[f].members.iter().filter(|m| m.deleted && uids.contains(&m.uid)).map(|m| m.message).collect();
        let for_good = self.gmail && matches!(self.folders[f].special, "\\Trash" | "\\Junk" | "\\Drafts" | "\\All");
        for m in gone {
            self.take_out(f, m);
            if for_good {
                for folder in &mut self.folders {
                    folder.members.retain(|x| x.message != m);
                }
                self.messages[m] = None;
            }
        }
    }

    /// Whether a member matches a SEARCH's criteria (the ones Envoy sends).
    fn matches(&self, m: &Member, criteria: &[String]) -> bool {
        let Some(x) = self.messages[m.message].as_ref() else { return false };
        let fl = self.flags_of(m);
        let mut i = 0;
        while i < criteria.len() {
            let ok = match criteria[i].to_ascii_uppercase().as_str() {
                "UNDELETED" => fl & flags::DELETED == 0,
                "DELETED" => fl & flags::DELETED != 0,
                "UNSEEN" => fl & flags::SEEN == 0,
                "SEEN" => fl & flags::SEEN != 0,
                "HEADER" => {
                    let (name, value) = (criteria.get(i + 1).cloned().unwrap_or_default(), criteria.get(i + 2).cloned().unwrap_or_default().to_lowercase());
                    i += 2;
                    let found = header_fields(&x.raw, Some(&[name]));
                    String::from_utf8_lossy(&found).split_once(':').is_some_and(|(_, v)| v.to_lowercase().contains(&value))
                }
                _ => true,
            };
            if !ok {
                return false;
            }
            i += 1;
        }
        true
    }

    /// FETCH's answer for these members of folder `f`, the items asked for. True when it
    /// marked any read (a `BODY[]` without `.PEEK`).
    fn fetch(&mut self, f: usize, uids: &[u32], items: &str, uid: bool, out: &mut Vec<u8>) -> bool {
        let items = items.strip_prefix('(').and_then(|i| i.strip_suffix(')')).unwrap_or(items);
        let mut items = words(items);
        if uid && !items.iter().any(|i| i.eq_ignore_ascii_case("UID")) {
            items.insert(0, "UID".into());
        }
        let mut seen = false;
        for u in uids {
            let Some((seq, member)) = self.folders[f].members.iter().enumerate().find(|(_, m)| m.uid == *u) else { continue };
            let m = member.message;
            let Some(x) = self.messages[m].as_ref() else { continue };
            let raw = x.raw.clone();
            let mut parts: Vec<Vec<u8>> = Vec::new();
            let mut mark = false;
            for item in &items {
                let upper = item.to_ascii_uppercase();
                let part = match upper.as_str() {
                    "UID" => format!("UID {u}").into_bytes(),
                    "FLAGS" => format!("FLAGS ({})", imap::flag_names(self.flags_of(member))).into_bytes(),
                    "INTERNALDATE" => {
                        let at = chrono::DateTime::from_timestamp(x.date, 0).unwrap_or_default();
                        format!("INTERNALDATE \"{}\"", at.format("%d-%b-%Y %H:%M:%S +0000")).into_bytes()
                    }
                    "RFC822.SIZE" => format!("RFC822.SIZE {}", raw.len()).into_bytes(),
                    _ => match upper.strip_prefix("BODY.PEEK[").or_else(|| upper.strip_prefix("BODY[")).and_then(|s| s.strip_suffix(']')) {
                        Some(section) => {
                            mark |= upper.starts_with("BODY[");
                            let bytes = match section {
                                "" => raw.to_vec(),
                                "HEADER" => header(&raw).to_vec(),
                                "TEXT" => raw[header(&raw).len()..].to_vec(),
                                s => match s.strip_prefix("HEADER.FIELDS") {
                                    Some(names) => header_fields(&raw, Some(&words(names.trim().trim_start_matches('(').trim_end_matches(')')))),
                                    None => raw.to_vec(),
                                },
                            };
                            let mut p = format!("BODY[{section}] {{{}}}\r\n", bytes.len()).into_bytes();
                            p.extend_from_slice(&bytes);
                            p
                        }
                        None => continue,
                    },
                };
                parts.push(part);
            }
            out.extend_from_slice(format!("* {} FETCH (", seq + 1).as_bytes());
            out.extend_from_slice(&parts.join(&b' '));
            out.extend_from_slice(b")\r\n");
            if mark {
                self.flag(m, flags::SEEN, true);
                seen = true;
            }
        }
        seen
    }
}

/// A message's header block, blank line included.
fn header(raw: &[u8]) -> &[u8] {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => &raw[..i + 4],
        None => match raw.windows(2).position(|w| w == b"\n\n") {
            Some(i) => &raw[..i + 2],
            None => raw,
        },
    }
}

/// The header fields named (folded lines and all), as `BODY[HEADER.FIELDS (…)]` gives them,
/// ending in a blank line; all of them for `None`.
fn header_fields(raw: &[u8], names: Option<&[String]>) -> Vec<u8> {
    let text = String::from_utf8_lossy(header(raw)).into_owned();
    let mut out = String::new();
    let mut keep = false;
    for line in text.split_inclusive('\n') {
        if line.trim_end().is_empty() {
            break;
        }
        if !line.starts_with([' ', '\t']) {
            let name = line.split(':').next().unwrap_or("").trim();
            keep = names.is_none_or(|n| n.iter().any(|w| w.eq_ignore_ascii_case(name)));
        }
        if keep {
            out.push_str(line.trim_end_matches(['\r', '\n']));
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Flag names in a list (`(\Seen $new)`) as [`flags`] bits.
fn flag_bits(list: &str) -> u32 {
    let list = list.trim().trim_start_matches('(').trim_end_matches(')');
    list.split_whitespace()
        .map(|f| match f.to_ascii_lowercase().as_str() {
            "\\seen" => flags::SEEN,
            "\\flagged" => flags::FLAGGED,
            "\\answered" => flags::ANSWERED,
            "\\draft" => flags::DRAFT,
            "\\deleted" => flags::DELETED,
            k if k == crate::snooze::NEW_KEYWORD => flags::WOKEN,
            _ => 0,
        })
        .fold(0, |a, b| a | b)
}

/// Whether `n` is in IMAP sequence set `set` (`1:4,7,9:*`), `*` standing for `max`.
fn in_set(set: &str, n: u32, max: u32) -> bool {
    let num = |s: &str| if s == "*" { Some(max) } else { s.parse::<u32>().ok() };
    set.split(',').any(|part| match part.split_once(':') {
        Some((a, b)) => match (num(a), num(b)) {
            (Some(a), Some(b)) => (a.min(b)..=a.max(b)).contains(&n),
            _ => false,
        },
        None => num(part) == Some(n),
    })
}

/// A command's arguments: atoms, quoted strings (unquoted), and parenthesized or bracketed
/// parts kept whole, as they are.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        let mut word = String::new();
        if c == '"' {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => word.extend(chars.next()),
                    '"' => break,
                    c => word.push(c),
                }
            }
        } else {
            let mut depth = 0i32;
            while let Some(&c) = chars.peek() {
                if c == ' ' && depth == 0 {
                    break;
                }
                depth += match c {
                    '(' | '[' => 1,
                    ')' | ']' => -1,
                    _ => 0,
                };
                word.push(c);
                chars.next();
            }
        }
        out.push(word);
    }
    out
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AuthKind;

    fn account(id: &str, email: &str, host: &str) -> Account {
        Account {
            id: id.into(),
            email: email.into(),
            display_name: String::new(),
            auth: AuthKind::Password,
            username: email.into(),
            imap_host: host.into(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            hue: 0.0,
            signature: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
        }
    }

    const GMAIL: &[(&str, &str)] = &[("INBOX", ""), ("[Gmail]/All Mail", "\\All"), ("[Gmail]/Starred", "\\Flagged"), ("[Gmail]/Trash", "\\Trash"), ("[Gmail]/Sent Mail", "\\Sent"), ("Travel", "")];

    fn mail(id: &str) -> Vec<u8> {
        format!("From: a@x.example\r\nTo: me@gmail.com\r\nSubject: {id}\r\nMessage-ID: <{id}@x>\r\nDate: Tue, 3 Mar 2026 10:00:00 +0000\r\n\r\nHello\r\n").into_bytes()
    }

    fn ids(server: &Server, folder: &str) -> Vec<String> {
        server.contents("g", folder).into_iter().map(|(_, m)| m).collect()
    }

    #[test]
    fn sets_words_and_header_fields() {
        assert!(in_set("1:3,7", 2, 9) && in_set("1:3,7", 7, 9) && !in_set("1:3,7", 5, 9));
        assert!(in_set("5:*", 9, 9) && in_set("12:*", 9, 9), "n:* past the end names the last, as RFC 3501 says");
        assert_eq!(words("\"\" \"a \\\"b\\\"\" (UID BODY.PEEK[HEADER.FIELDS (FROM TO)]) {12}"), ["", "a \"b\"", "(UID BODY.PEEK[HEADER.FIELDS (FROM TO)])", "{12}"]);
        let raw = b"From: a@x\r\nSubject: one\r\n two\r\nTo: b@x\r\n\r\nbody";
        assert_eq!(header_fields(raw, Some(&["subject".into(), "TO".into()])), b"Subject: one\r\n two\r\nTo: b@x\r\n\r\n");
        assert_eq!(flag_bits("(\\Seen $new \\Deleted)"), flags::SEEN | flags::WOKEN | flags::DELETED);
    }

    #[test]
    fn gmail_folders_are_labels_and_views() {
        let server = Server::new();
        server.add_account(account("g", "me@gmail.com", "imap.gmail.com"), GMAIL);
        server.add_mail("g", &["INBOX"], mail("a"), 0, 0);
        server.add_mail("g", &["INBOX", "Travel"], mail("b"), flags::FLAGGED, 0);
        let mut data = server.lock();
        let s = &mut data[0];
        assert_eq!(s.folders[1].members.len(), 2, "All Mail has both");
        assert_eq!(s.folders[2].members.len(), 1, "Starred has the starred one");
        // Archived: out of the inbox, still in All Mail with its UID.
        let (inbox, all, starred, trash, travel) = (0, 1, 2, 3, 5);
        let all_uid = s.folders[all].members[0].uid;
        s.move_one(inbox, 1, all);
        s.settle();
        assert_eq!((s.folders[inbox].members.len(), s.folders[all].members[0].uid), (1, all_uid));
        // Starring is shared: in Starred, wherever it's starred from.
        s.store(all, all_uid, "+FLAGS.SILENT", flags::FLAGGED);
        s.settle();
        assert_eq!(s.folders[starred].members.len(), 2);
        // Trash takes it out of its labels and the views.
        let b = s.folders[travel].members[0].uid;
        s.move_one(travel, b, trash);
        s.settle();
        assert!(s.folders[inbox].members.is_empty() && s.folders[travel].members.is_empty());
        assert_eq!((s.folders[all].members.len(), s.folders[starred].members.len(), s.folders[trash].members.len()), (1, 1, 1));
        // Deleted forever from Trash.
        let t = s.folders[trash].members[0].uid;
        s.store(trash, t, "+FLAGS.SILENT", flags::DELETED);
        s.expunge(trash, &[t]);
        s.settle();
        assert!(s.folders[trash].members.is_empty() && s.messages[1].is_none());
    }

    #[tokio::test]
    async fn the_engines_imap_code_runs_against_it() {
        let server = Arc::new(Server::new());
        let me = account("g", "me@gmail.com", "imap.gmail.com");
        server.add_account(me.clone(), GMAIL);
        server.add_mail("g", &["INBOX"], mail("a"), 0, 1_700_000_000);
        server.add_mail("g", &["INBOX"], mail("b"), flags::SEEN, 1_700_000_100);
        let store = Mutex::new(crate::store::Store::memory());
        let mut s = server.connect(&me).await.unwrap();
        let caps = imap::capabilities(&mut s).await.unwrap();
        assert!(caps.moves && caps.idle && caps.uidplus);
        let listed = imap::list_mailboxes(&mut s).await.unwrap();
        assert!(listed.iter().any(|m| m.name == "[Gmail]/All Mail" && m.role == crate::model::Role::All));
        for m in &listed {
            store.lock().unwrap().upsert_mailbox("g", &m.name, &m.display, m.role, &m.delimiter, m.selectable).unwrap();
        }
        let out = imap::sync_mailbox(&mut s, &store, "g", "INBOX", 50, &|| {}).await.unwrap();
        assert_eq!(out.new_messages, 2);
        let validity = store.lock().unwrap().uidvalidity("g", "INBOX").unwrap();
        assert_eq!(imap::fetch_bodies(&mut s, &store, "g", "INBOX", validity, &[1, 2]).await.unwrap().len(), 2);
        assert_eq!(imap::status(&mut s, "INBOX").await.unwrap(), (1, 2));
        // Moved and labeled as the op queue does it.
        let target = imap::Target { uid: 1, validity, message_id: "<a@x>" };
        imap::copy_message(&mut s, "INBOX", &target, "Travel").await.unwrap();
        imap::move_message(&mut s, caps, "INBOX", &target, "[Gmail]/All Mail").await.unwrap();
        assert_eq!(ids(&server, "INBOX"), ["<b@x>"]);
        assert_eq!(ids(&server, "Travel"), ["<a@x>"]);
        // A draft replaced by its next save.
        imap::append(&mut s, "Travel", &mail("c"), true).await.unwrap();
        let replaced = imap::replace_draft(&mut s, caps, "Travel", &mail("c"), "<c@x>", None, true).await.unwrap();
        assert!(replaced.certain && replaced.deleted.len() == 1, "{replaced:?}");
        assert_eq!(ids(&server, "Travel"), ["<a@x>", "<c@x>"]);
        // A snooze's bucket, and its wake.
        imap::ensure_bucket(&mut s, "Snoozed", "Snoozed/20260101T0000Z").await.unwrap();
        let b = imap::Target { uid: 2, validity, message_id: "<b@x>" };
        imap::move_message(&mut s, caps, "INBOX", &b, "Snoozed/20260101T0000Z").await.unwrap();
        let woken = imap::wake_bucket(&mut s, caps, "Snoozed/20260101T0000Z", "INBOX").await.unwrap();
        assert_eq!(woken.message_ids, ["<b@x>"]);
        assert_eq!(server.contents("g", "INBOX"), [(flags::WOKEN, "<b@x>".to_owned())], "back unread, marked");
        // The sync finds what changed.
        let out = imap::sync_mailbox(&mut s, &store, "g", "INBOX", 50, &|| {}).await.unwrap();
        assert!(out.changed);
        s.logout().await.unwrap();
    }

    #[tokio::test]
    async fn idle_hears_of_mail_delivered_meanwhile() {
        let server = Arc::new(Server::new());
        let me = account("g", "me@gmail.com", "imap.gmail.com");
        server.add_account(me.clone(), GMAIL);
        let mut s = server.connect(&me).await.unwrap();
        imap::select(&mut s, "INBOX").await.unwrap();
        let deliver = {
            let server = server.clone();
            let me = me.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                server.deliver(&me, &mail("hi"), &["Me@Gmail.com".into()]);
            })
        };
        let (_s, news) = imap::idle(s, std::time::Duration::from_secs(5), Arc::new(tokio::sync::Notify::new())).await.unwrap();
        deliver.await.unwrap();
        assert!(news, "told of the new mail");
        assert_eq!(ids(&server, "INBOX"), ["<hi@x>"]);
        assert_eq!(ids(&server, "[Gmail]/Sent Mail"), ["<hi@x>"], "Gmail files what it sends");
    }
}
