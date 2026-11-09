//! Drafts as the engine handles them: saving the composer's content, and the Drafts list, which
//! shows each local draft with its newest content, before its server copy exists and while
//! that copy is behind. The local table and its states are in `store/drafts.rs`; uploads and
//! deletions replay through the op queue ([`Op::SaveDraft`], [`Op::PurgeDraft`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::model::{Address, Draft, MessageId, Role, Summary, View, flags};
#[cfg(doc)]
use crate::store::Op;
use crate::store::{DraftRev, LocalDraft, Query, Store};

/// Seconds a draft being written waits before its upload, so typing makes an upload every few
/// seconds rather than one per save. A draft put away (another one took the composer) goes now.
pub const UPLOAD_DELAY: i64 = 5;

/// How long a draft being written waits before its upload ([`UPLOAD_DELAY`]), longer the more
/// its attached files weigh (`attached`, bytes): each upload sends them all again (IMAP
/// replaces a draft whole), so a draft carrying 25 MB is uploaded every two minutes while it's
/// typed in, not every five seconds.
pub fn upload_delay(attached: usize) -> i64 {
    let mb = (attached / (1024 * 1024)) as i64;
    UPLOAD_DELAY + (mb * 4).min(115)
}

/// Saves the composer's content ([`Store::save_draft`]) and, if the server's copy is now
/// behind, queues its upload. True when the queue changed, so the account's worker should
/// plan its wait again; None when the store refused the save.
pub fn save(st: &Store, id: i64, draft: &Draft, open: bool, at: DraftRev, now: i64) -> Result<Option<bool>> {
    save_from(st, id, draft, open, at, now, None)
}

/// [`save`], for a draft that may carry on another's text (`files_from`, see
/// [`Store::save_draft_from`]).
pub fn save_from(st: &Store, id: i64, draft: &Draft, open: bool, at: DraftRev, now: i64, files_from: Option<i64>) -> Result<Option<bool>> {
    let Some(changed) = st.save_draft_from(id, draft, open, at, now, files_from)? else { return Ok(None) };
    if (changed || !open) && st.draft_needs_upload(id)? {
        return st.queue_draft_upload(&draft.account, id, if open { now + upload_delay(st.attached_size(id)?) } else { now }).map(Some);
    }
    Ok(Some(false))
}

/// Saves `draft` as a new draft ([`Store::insert_new_draft`]) and queues its upload, in one
/// transaction; its files are those of draft `files_from`. Returns its id.
pub fn save_new(st: &Store, draft: &Draft, open: bool, rev: i64, now: i64, files_from: Option<i64>) -> Result<i64> {
    st.transaction(|st| {
        let id = st.insert_new_draft(draft, open, rev, now)?;
        st.take_files_from(files_from, id, &draft.attachments)?;
        st.queue_draft_upload(&draft.account, id, if open { now + upload_delay(st.attached_size(id)?) } else { now })?;
        Ok(id)
    })
}

/// A draft the app couldn't write to the store as the window closed: kept in a file of its own
/// in the recovery folder ([`recovery_dir`]) and imported on the next start, so it's never lost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recovered {
    pub id: i64,
    pub draft: Draft,
    /// It was in the composer.
    pub open: bool,
    /// The app's stamp when it was written (or sent): imported like a save made then, so it
    /// doesn't overwrite anything newer, unsent messages included.
    #[serde(alias = "stamp")]
    pub rev: i64,
    /// The draft's lifecycle it was written in: it brings back nothing that ended since.
    #[serde(default)]
    pub lifecycle: i64,
    /// A message sent but not confirmed queued: kept as a draft unless its send went through.
    pub unsent: bool,
    /// It carries on draft `files_from`'s text (a write refused under that id), and the files
    /// it lists are that draft's ([`Store::save_draft_from`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_from: Option<i64>,
}

/// Where [`Recovered`] drafts wait for the next start: a folder, one file per draft.
pub fn recovery_dir() -> PathBuf {
    crate::paths::data_dir().join("drafts-recovery")
}

/// Puts each draft in a file of its own in the recovery folder `dir`. Names are unique (stamp,
/// draft, process, random), so writers never touch each other's files, whichever Envoy they're
/// in; each is written under a temporary name, flushed to disk, then renamed, so an importer
/// only ever sees whole files. Tries every draft: an error if any couldn't be written.
pub fn save_recovery(dir: &Path, drafts: &[Recovered]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut failed = None;
    for r in drafts {
        if let Err(e) = write_recovered(dir, r) {
            failed.get_or_insert(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

fn write_recovered(dir: &Path, r: &Recovered) -> Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(r)?;
    let name = format!("{:020}-{}-{}-{:016x}", r.rev.max(0), r.id, std::process::id(), rand::random::<u64>());
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let written = f.write_all(&bytes).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, dir.join(format!("{name}.json")))) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Imports the drafts waiting in the recovery folder `dir` (none if there's no folder), oldest
/// first, one file at a time: each like a save made when it was written, and its file is
/// removed only once the store holds its text. One the store refuses (its draft ended or moved
/// on since), or that newer text overtook, becomes a draft of its own. Files written meanwhile, by this Envoy or
/// another, wait for the next import; one another importer took first is skipped. A file that
/// isn't a draft is set aside (renamed), never removed. Stops at the first store error, leaving
/// the rest for the next start. Returns how many were imported.
///
/// Each import is on disk before its file goes ([`Store::durably`]): a power cut can't take
/// back the text and keep the file's deletion.
pub fn import_recovery(dir: &Path, st: &Store, now: i64) -> Result<usize> {
    st.durably(|st| import_files(dir, st, now))
}

fn import_files(dir: &Path, st: &Store, now: i64) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    // Whole files only: ones still being written have a temporary name.
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    files.sort();
    let mut imported = 0;
    for file in files {
        let r: Recovered = match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    log::error!("{} isn't a draft ({e}): set aside", file.display());
                    if let Err(e) = std::fs::rename(&file, file.with_extension("unreadable")) {
                        log::error!("couldn't set {} aside: {e}", file.display());
                    }
                    continue;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        import_one(st, &r, now)?;
        // It's in the store now: its file can go.
        match std::fs::remove_file(&file) {
            Ok(()) => imported += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(imported)
}

/// One recovered draft into the store, like a save made when it was written. Only its text in
/// the store (or, for an unsent message, its send on its way) lets its file go: refused, or
/// overtaken by other text, it's kept as a new draft, with the files of the draft it was
/// written for (the store's: a recovered draft's text is what's recovered).
fn import_one(st: &Store, r: &Recovered, now: i64) -> Result<()> {
    let at = DraftRev { lifecycle: r.lifecycle, rev: r.rev };
    let files = r.files_from.unwrap_or(r.id);
    if r.unsent {
        st.draft_unsent(r.id, &r.draft, Some(at), now)?;
    } else {
        save_from(st, r.id, &r.draft, r.open, at, now, r.files_from)?;
    }
    let text = |d: &Draft| Draft { attachments: Vec::new(), ..d.clone() };
    let held = st.draft(r.id)?.is_some_and(|d| text(&d.draft) == text(&r.draft)) || (r.unsent && st.draft_on_its_way(r.id)?);
    if !held {
        save_new(st, &r.draft, r.open, r.rev, now, Some(files))?;
    }
    Ok(())
}

/// A Drafts list row for a local draft the server doesn't have yet: negative, so it's never a
/// message's id.
pub fn list_id(draft: i64) -> MessageId {
    -draft
}

/// The local draft a Drafts list row with this id stands for, if it's one of [`list_id`]'s.
pub fn draft_of_list_id(id: MessageId) -> Option<i64> {
    (id < 0).then(|| -id)
}

/// The account a Drafts view shows (`Some(None)`: every account), or None when it isn't one.
fn drafts_view<'a>(st: &Store, view: &'a View) -> Option<Option<&'a str>> {
    match view {
        View::Role { role: Role::Drafts, account } => Some(account.as_deref()),
        View::Mailbox { account, mailbox } if st.role_of(account, mailbox) == Role::Drafts => Some(Some(account)),
        _ => None,
    }
}

/// Whether `view` is a Drafts list, whose rows [`merge`] merges with the local drafts.
pub fn lists_drafts(st: &Store, view: &View) -> bool {
    drafts_view(st, view).is_some()
}

/// The list for a Drafts view: `items`, the server's copies from [`Store::list`], with copies of
/// local drafts showing the draft's newest content, copies of drafts being sent or discarded left
/// out, and local drafts the server doesn't have yet added (ids from [`list_id`]). Newest first,
/// at most `limit`. Any other view's list comes back as it was. `from` names an account's
/// sender, for rows that have no server copy to take it from.
pub fn merge(st: &Store, view: &View, query: &Query, limit: u32, mut items: Vec<Summary>, from: impl Fn(&str) -> Address) -> Result<Vec<Summary>> {
    let Some(account) = drafts_view(st, view) else { return Ok(items) };
    let drafts: HashMap<i64, LocalDraft> = st.drafts(account)?.into_iter().map(|d| (d.id, d)).collect();
    if drafts.is_empty() {
        return Ok(items);
    }
    let copies: HashMap<MessageId, i64> = st.draft_copies(account)?.into_iter().collect();
    let mut on_server = HashSet::new();
    items.retain_mut(|row| {
        let Some(d) = copies.get(&row.id).and_then(|d| drafts.get(d)) else { return true };
        on_server.insert(d.id);
        if !d.visible {
            return false;
        }
        show(row, d);
        true
    });
    for d in drafts.values().filter(|d| d.visible && !on_server.contains(&d.id) && matches(query, &d.draft)) {
        let mailbox = st.mailbox_for_role(&d.account, Role::Drafts)?.unwrap_or_default();
        let mut row = Summary {
            id: list_id(d.id),
            account: d.account.clone(),
            mailbox,
            uid: 0,
            thread: format!("draft:{}", d.id),
            subject: String::new(),
            from: from(&d.account),
            to: Vec::new(),
            date: 0,
            flags: 0,
            preview: String::new(),
            has_attachments: false,
            thread_count: 1,
            participants: String::new(),
            thread_unread: false,
        };
        show(&mut row, d);
        items.push(row);
    }
    // Ordered like every list (store/list.rs): newest first, then the higher id.
    items.sort_by_key(|r| std::cmp::Reverse((r.date, r.id)));
    items.truncate(limit as usize);
    Ok(items)
}

/// Puts a local draft's content on its list row.
fn show(row: &mut Summary, d: &LocalDraft) {
    let draft = &d.draft;
    row.subject = draft.subject.clone();
    row.to = [&draft.to, &draft.cc].into_iter().flat_map(|f| people(f)).collect();
    row.preview = crate::mime::preview(Some(&draft.body), None);
    row.date = d.updated;
    row.flags |= flags::SEEN | flags::DRAFT;
    row.thread_unread = false;
}

/// The people in a recipients field, as far as it parses (it may be half typed).
fn people(field: &str) -> Vec<Address> {
    if field.trim().is_empty() {
        return Vec::new();
    }
    match crate::smtp::parse_recipients(field) {
        Ok(list) => list.into_iter().map(|m| Address { name: m.name.unwrap_or_default(), email: m.email.to_string() }).collect(),
        Err(_) => vec![Address { name: String::new(), email: field.trim().to_owned() }],
    }
}

/// Whether a local draft matches a search, for drafts the search index doesn't have: words in
/// its subject, recipients or text, and the `to:` and `subject:` filters. A draft is read, not
/// starred, from you and has no attachments, which settles the other filters.
fn matches(q: &Query, d: &Draft) -> bool {
    if q.unread == Some(true) || q.starred == Some(true) || q.attachment || !q.from.is_empty() {
        return false;
    }
    let all = format!("{} {} {} {} {}", d.subject, d.to, d.cc, d.bcc, d.body).to_lowercase();
    let to = format!("{} {}", d.to, d.cc).to_lowercase();
    let subject = d.subject.to_lowercase();
    q.text.iter().all(|w| all.contains(&w.to_lowercase())) && q.to.iter().all(|t| to.contains(t)) && q.subject.iter().all(|s| subject.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("envoy-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn recovered(id: i64, subject: &str, rev: i64, unsent: bool) -> Recovered {
        Recovered { id, draft: Draft { account: "a".into(), subject: subject.into(), ..Draft::default() }, open: true, rev, lifecycle: 0, unsent, files_from: None }
    }

    /// The files in `folder` (none if there's no folder).
    fn files(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn drafts_kept_in_the_recovery_folder_come_back_on_the_next_start() {
        let dir = temp("roundtrip");
        let folder = dir.join("drafts-recovery");
        assert_eq!(import_recovery(&folder, &Store::memory(), 100).unwrap(), 0, "no folder, nothing to do");
        save_recovery(&folder, &[recovered(1, "first close", 10, false)]).unwrap();
        save_recovery(&folder, &[recovered(2, "second close", 20, false), recovered(3, "sent, unconfirmed", 30, true)]).unwrap();
        assert_eq!(files(&folder).len(), 3, "a file each: {:?}", files(&folder));
        let st = Store::memory();
        assert_eq!(import_recovery(&folder, &st, 100).unwrap(), 3, "both closes kept");
        assert!(files(&folder).is_empty(), "done with: {:?}", files(&folder));
        let subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        assert_eq!(subjects.len(), 3, "{subjects:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recovered_drafts_file_goes_only_once_its_draft_is_in_the_store() {
        let dir = temp("committed");
        let folder = dir.join("drafts-recovery");
        save_recovery(&folder, &[recovered(1, "one", 10, false), recovered(2, "two", 20, false)]).unwrap();
        let broken = Store::memory();
        broken.break_drafts();
        assert!(import_recovery(&folder, &broken, 100).is_err());
        assert_eq!(files(&folder).len(), 2, "both kept for the next start");
        let st = Store::memory();
        assert_eq!(import_recovery(&folder, &st, 101).unwrap(), 2);
        assert!(files(&folder).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_drafts_overwrite_nothing_newer_and_revive_nothing_ended() {
        let dir = temp("rules");
        let file = dir.join("drafts-recovery");
        let st = Store::memory();
        let draft = |subject: &str| Draft { account: "a".into(), subject: subject.into(), ..Draft::default() };
        st.save_draft(1, &draft("newer"), true, DraftRev { lifecycle: 0, rev: 50 }, 100).unwrap();
        // Draft 2 ended (discarded for good), and 3 was sent, before their snapshots were
        // written (a too-late Undo had put 2 back in the composer).
        st.save_draft(2, &draft("discarded"), true, DraftRev { lifecycle: 0, rev: 5 }, 100).unwrap();
        st.discard_draft(2, None).unwrap();
        let send = st.enqueue_at("a", &crate::store::Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 100).unwrap();
        st.transaction(|s| s.draft_sending(3, &draft("sent"), send, 100)).unwrap();
        st.op_done(send).unwrap();
        save_recovery(&file, &[recovered(1, "older", 40, false), recovered(2, "typed after the late undo", 6, false), recovered(3, "sent", 7, true)]).unwrap();
        import_recovery(&file, &st, 101).unwrap();
        assert_eq!(st.draft(1).unwrap().unwrap().draft.subject, "newer");
        assert_eq!((st.draft(2).unwrap(), st.draft(3).unwrap()), (None, None), "neither comes back");
        let mut subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        subjects.sort();
        // The texts that couldn't go where they were written are kept as new drafts.
        assert_eq!(subjects, vec!["newer", "older", "typed after the late undo"], "{subjects:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_drafts_list_in_the_stores_order() {
        let st = Store::memory();
        for id in 1..=8 {
            st.save_draft(id, &Draft { account: "a".into(), subject: format!("d{id}"), ..Draft::default() }, true, DraftRev { lifecycle: 0, rev: 1 }, 100).unwrap();
        }
        let view = View::Role { account: None, role: Role::Drafts };
        let rows = merge(&st, &view, &Query::default(), 100, vec![], |_| Address { name: String::new(), email: String::new() }).unwrap();
        // Same second: newest id first, as every list is ordered (store/list.rs).
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), (1..=8).map(list_id).collect::<Vec<_>>());
    }

    #[test]
    fn concurrent_writers_and_an_importer_lose_no_snapshot() {
        let dir = temp("concurrent");
        let path = dir.join("drafts-recovery");
        let (writers, each) = (4i64, 25i64);
        let done = std::sync::atomic::AtomicBool::new(false);
        let (imported, st) = std::thread::scope(|s| {
            let handles: Vec<_> = (0..writers)
                .map(|w| {
                    let path = &path;
                    s.spawn(move || (0..each).map(|i| save_recovery(path, &[recovered(1 + w * each + i, "kept", 10, false)]).map_err(|e| format!("{e:#}"))).collect::<Vec<_>>())
                })
                .collect();
            // Another Envoy starting meanwhile imports what's there, again and again.
            let importer = s.spawn(|| {
                let st = Store::memory();
                let mut n = 0;
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    n += import_recovery(&path, &st, 100).unwrap_or(0);
                }
                (n, st)
            });
            let failed: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).filter_map(|r| r.err()).collect();
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            let (n, st) = importer.join().unwrap();
            assert!(failed.is_empty(), "every write lands: {failed:?}");
            (n + import_recovery(&path, &st, 100).unwrap(), st)
        });
        assert_eq!(st.drafts(None).unwrap().len() as i64, writers * each, "every snapshot imported");
        assert_eq!(imported as i64, writers * each, "each once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recovered_draft_overtaken_by_other_text_is_kept_as_its_own() {
        let dir = temp("overtaken");
        let folder = dir.join("drafts-recovery");
        let st = Store::memory();
        save_recovery(&folder, &[recovered(1, "written at close", 40, false)]).unwrap();
        // Saved since with other text, stamped later.
        st.save_draft(1, &Draft { account: "a".into(), subject: "saved since".into(), ..Draft::default() }, true, DraftRev { lifecycle: 0, rev: 50 }, 100).unwrap();
        import_recovery(&folder, &st, 101).unwrap();
        assert_eq!(st.draft(1).unwrap().unwrap().draft.subject, "saved since", "nothing newer overwritten");
        let subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        assert!(subjects.contains(&"written at close".to_owned()), "and the file's text is in the store before the file goes: {subjects:?}");
        assert!(files(&folder).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importers_running_at_once_keep_every_refused_draft() {
        let dir = temp("importers");
        let db = dir.join("mail.sqlite");
        let (importers, each) = (4i64, 30i64);
        // Every snapshot's draft ended since it was written: each is kept as a new draft.
        {
            let st = Store::open(&db).unwrap();
            for id in 1..=importers * each {
                st.save_draft(id, &Draft { account: "a".into(), ..Draft::default() }, true, DraftRev { lifecycle: 0, rev: 1 }, 100).unwrap();
                st.discard_draft(id, None).unwrap();
            }
        }
        let folders: Vec<PathBuf> = (0..importers)
            .map(|i| {
                let folder = dir.join(format!("recovery-{i}"));
                let snapshots: Vec<Recovered> = (0..each).map(|j| recovered(1 + i * each + j, &format!("text {i}-{j}"), 10 + j, false)).collect();
                save_recovery(&folder, &snapshots).unwrap();
                folder
            })
            .collect();
        // Envoys starting at once, each importing its own snapshots into the same database, in
        // one go: each waits its turn for the database, never giving up on a busy one.
        let start = std::sync::Barrier::new(folders.len());
        let results: Vec<Result<usize, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = folders
                .iter()
                .map(|folder| {
                    let (db, start) = (&db, &start);
                    s.spawn(move || {
                        let st = Store::open(db).unwrap();
                        start.wait();
                        import_recovery(folder, &st, 100).map_err(|e| format!("{e:#}"))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results, vec![Ok(each as usize); importers as usize], "every importer took all of its snapshots");
        assert!(folders.iter().all(|f| files(f).is_empty()), "and their files went");
        let st = Store::open(&db).unwrap();
        let mut subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        subjects.sort();
        let mut want: Vec<String> = (0..importers).flat_map(|i| (0..each).map(move |j| format!("text {i}-{j}"))).collect();
        want.sort();
        assert_eq!(subjects, want, "each snapshot's text kept, once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recovered_unsent_message_overwrites_nothing_newer() {
        let dir = temp("unsent");
        let file = dir.join("drafts-recovery");
        let st = Store::memory();
        save_recovery(&file, &[recovered(1, "sent, never queued", 40, true)]).unwrap();
        // Edited since, in a later run (the file stayed: the database refused it then).
        st.save_draft(1, &Draft { account: "a".into(), subject: "edited since".into(), ..Draft::default() }, true, DraftRev { lifecycle: 0, rev: 50 }, 100).unwrap();
        import_recovery(&file, &st, 101).unwrap();
        assert_eq!(st.draft(1).unwrap().unwrap().draft.subject, "edited since");
        let subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        assert!(subjects.contains(&"sent, never queued".to_owned()), "kept as a new draft: {subjects:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Attaches `data` to draft `id` as file `file`, as the engine does.
    fn attach(st: &Store, id: i64, file: i64, name: &str, data: &[u8], now: i64) {
        let listed = crate::model::DraftAttachment { id: file, filename: name.into(), status: crate::model::FileStatus::Attaching, ..Default::default() };
        st.attaching(id, &[(listed, crate::model::FileSource::Path(name.into()))]).unwrap();
        st.file_in(file, data, "", now).unwrap();
    }

    #[test]
    fn a_heavy_draft_is_uploaded_less_often_while_its_typed_in() {
        assert_eq!(upload_delay(0), UPLOAD_DELAY);
        assert_eq!(upload_delay(1 << 20), 9);
        assert_eq!(upload_delay(25 << 20), 105, "a full message: every couple of minutes");
        assert_eq!(upload_delay(400 << 20), 120, "never longer");
        // As queued: a file coming in, or a save, waits that long; putting the draft away still
        // uploads it now.
        let st = Store::memory();
        let draft = Draft { account: "a".into(), ..Draft::default() };
        save(&st, 1, &draft, true, DraftRev { lifecycle: 0, rev: 1 }, 0).unwrap();
        let first = st.claim_due("a", 10).unwrap();
        st.op_done(first[0].id).unwrap();
        attach(&st, 1, 9, "f", &vec![0u8; 2 * 1024 * 1024], 1000);
        assert!(st.ops("a", 1000 + UPLOAD_DELAY).unwrap().is_empty(), "not after five seconds");
        assert_eq!(st.ops("a", 1013).unwrap().len(), 1);
        save(&st, 1, &draft, false, DraftRev { lifecycle: 0, rev: 3 }, 1001).unwrap();
        assert_eq!(st.ops("a", 1001).unwrap().len(), 1, "put away: now");
    }

    #[test]
    fn a_recovered_draft_kept_as_its_own_keeps_its_files() {
        let dir = temp("files");
        let folder = dir.join("drafts-recovery");
        let st = Store::memory();
        let text = |subject: &str| Draft { account: "a".into(), subject: subject.into(), ..Draft::default() };
        st.save_draft(1, &text(""), true, DraftRev { lifecycle: 0, rev: 1 }, 100).unwrap();
        attach(&st, 1, 9, "plan.pdf", b"%PDF", 100);
        // Written at close, then other text saved over it in a later run.
        let mut r = recovered(1, "", 40, false);
        r.draft = text("written at close");
        save_recovery(&folder, &[r]).unwrap();
        st.save_draft(1, &text("saved since"), true, DraftRev { lifecycle: 0, rev: 50 }, 100).unwrap();
        import_recovery(&folder, &st, 101).unwrap();
        let kept = st.drafts(None).unwrap().into_iter().find(|d| d.draft.subject == "written at close").expect("kept as its own");
        assert_ne!(kept.id, 1);
        assert_eq!(st.attached_files(kept.id).unwrap()[0].1, b"%PDF".to_vec(), "with the file");
        // One written for a draft carrying on another's text takes that one's files.
        let mut r = recovered(7, "", 60, false);
        r.draft = text("carried on");
        r.files_from = Some(1);
        save_recovery(&folder, &[r]).unwrap();
        import_recovery(&folder, &st, 102).unwrap();
        assert_eq!(st.attached_files(7).unwrap()[0].1, b"%PDF".to_vec());
        assert_eq!(st.drafts(None).unwrap().len(), 3, "and nothing more");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_recovery_file_is_set_aside_not_removed() {
        let dir = temp("unreadable");
        let folder = dir.join("drafts-recovery");
        save_recovery(&folder, &[recovered(1, "kept", 10, false)]).unwrap();
        std::fs::write(folder.join("0-damaged.json"), b"{ half written").unwrap();
        let st = Store::memory();
        assert_eq!(import_recovery(&folder, &st, 100).unwrap(), 1, "the others still come in");
        assert_eq!(files(&folder), vec!["0-damaged.unreadable".to_owned()], "set aside, kept");
        assert_eq!(import_recovery(&folder, &st, 100).unwrap(), 0, "and not tried again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
