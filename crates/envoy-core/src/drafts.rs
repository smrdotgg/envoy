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
use crate::store::{LocalDraft, Query, Store};

/// Seconds a draft being written waits before its upload, so typing makes an upload every few
/// seconds rather than one per save. A draft put away (another one took the composer) goes now.
pub const UPLOAD_DELAY: i64 = 5;

/// Saves the composer's content ([`Store::save_draft`]) and, if the server's copy is now
/// behind, queues its upload. True when the queue changed, so the account's worker should
/// plan its wait again.
pub fn save(st: &Store, id: i64, draft: &Draft, open: bool, rev: i64, now: i64) -> Result<bool> {
    let changed = st.save_draft(id, draft, open, rev, now)?;
    if (changed || !open) && st.draft_needs_upload(id)? {
        return st.queue_draft_upload(&draft.account, id, if open { now + UPLOAD_DELAY } else { now });
    }
    Ok(false)
}

/// A draft the app couldn't write to the store as the window closed: kept in a plain file
/// ([`recovery_file`]) and imported on the next start, so it's never lost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recovered {
    pub id: i64,
    pub draft: Draft,
    /// It was in the composer.
    pub open: bool,
    /// When it was written (or sent): imported like a save made then, so it neither
    /// overwrites anything newer nor brings back a draft that's gone since.
    pub stamp: i64,
    /// A message sent but not confirmed queued: kept as a draft unless its send went through.
    pub unsent: bool,
}

/// Where [`Recovered`] drafts wait for the next start.
pub fn recovery_file() -> PathBuf {
    crate::paths::data_dir().join("drafts-recovery.json")
}

/// Adds drafts to the recovery file at `path`, with any already waiting there, replacing it
/// atomically. A file that can't be read is set aside, never overwritten.
pub fn save_recovery(path: &Path, drafts: &[Recovered]) -> Result<()> {
    let mut all: Vec<Recovered> = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(waiting) => waiting,
            Err(_) => {
                std::fs::rename(path, path.with_extension(format!("unreadable-{}", chrono::Utc::now().timestamp())))?;
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    all.extend(drafts.iter().cloned());
    crate::paths::write_atomic(path, &serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

/// Imports the drafts waiting in the recovery file at `path` (none if there's no file), each
/// like a save made when it was written, then removes the file. On any failure the file stays,
/// for the next start. Returns how many there were.
pub fn import_recovery(path: &Path, st: &Store, now: i64) -> Result<usize> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let waiting: Vec<Recovered> = serde_json::from_slice(&bytes)?;
    for r in &waiting {
        if r.unsent {
            st.draft_unsent(r.id, &r.draft, r.stamp, now)?;
        } else {
            save(st, r.id, &r.draft, r.open, r.stamp, now)?;
        }
    }
    std::fs::remove_file(path)?;
    Ok(waiting.len())
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
    items.sort_by_key(|r| std::cmp::Reverse(r.date));
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

    fn recovered(id: i64, subject: &str, stamp: i64, unsent: bool) -> Recovered {
        Recovered { id, draft: Draft { account: "a".into(), subject: subject.into(), ..Draft::default() }, open: true, stamp, unsent }
    }

    #[test]
    fn drafts_kept_in_the_recovery_file_come_back_on_the_next_start() {
        let dir = temp("roundtrip");
        let file = dir.join("drafts-recovery.json");
        save_recovery(&file, &[recovered(1, "first close", 10, false)]).unwrap();
        save_recovery(&file, &[recovered(2, "second close", 20, false), recovered(3, "sent, unconfirmed", 30, true)]).unwrap();
        let st = Store::memory();
        assert_eq!(import_recovery(&file, &st, 100).unwrap(), 3, "both closes kept");
        assert!(!file.exists(), "done with");
        let subjects: Vec<String> = st.drafts(None).unwrap().into_iter().map(|d| d.draft.subject).collect();
        assert_eq!(subjects.len(), 3, "{subjects:?}");
        assert_eq!(import_recovery(&file, &st, 100).unwrap(), 0, "no file, nothing to do");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_drafts_overwrite_nothing_newer_and_revive_nothing_gone() {
        let dir = temp("rules");
        let file = dir.join("drafts-recovery.json");
        let st = Store::memory();
        st.save_draft(1, &Draft { account: "a".into(), subject: "newer".into(), ..Draft::default() }, true, 50, 100).unwrap();
        // Draft 2 was discarded for good after its snapshot was taken.
        st.save_draft(2, &Draft { account: "a".into(), subject: "discarded".into(), ..Draft::default() }, true, 5, 100).unwrap();
        st.discard_draft(2, None).unwrap();
        save_recovery(&file, &[recovered(1, "older", 40, false), recovered(2, "discarded", 6, false)]).unwrap();
        import_recovery(&file, &st, 101).unwrap();
        assert_eq!(st.draft(1).unwrap().unwrap().draft.subject, "newer");
        assert_eq!(st.draft(2).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_recovery_file_is_set_aside_not_overwritten() {
        let dir = temp("unreadable");
        let file = dir.join("drafts-recovery.json");
        std::fs::write(&file, b"{ half written").unwrap();
        save_recovery(&file, &[recovered(1, "kept", 10, false)]).unwrap();
        let aside = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains("unreadable")).count();
        assert_eq!(aside, 1, "the old file is still there");
        let st = Store::memory();
        assert_eq!(import_recovery(&file, &st, 100).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
