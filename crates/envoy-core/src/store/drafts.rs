//! Drafts: what the composer holds, saved here as it's typed so a crash or quit never loses it,
//! and copied to the account's Drafts mailbox by [`Op::SaveDraft`].
//!
//! A draft is keyed by an id the app picks (microseconds since the epoch, so unique across runs)
//! and has a Message-ID of its own, which every server copy carries. That's how a save finds the
//! copies it replaces: the worker appends the newest version, then deletes every other copy with
//! that Message-ID, so the server ends up with one whatever was interrupted before, and needs
//! neither UIDPLUS nor the old copy's UID.
//!
//! A draft shows (in the composer, or in Drafts) unless it's on its way out:
//! - `sending`: the send op it waits in (undo send). Undo brings it back ([`Store::unsend_draft`]);
//!   once the send goes out, [`Store::draft_sent`] purges it.
//! - `purge_op`: the [`Op::PurgeDraft`] that will delete its server copies and then the draft,
//!   for a discarded or sent draft. A discard waits out the undo window, and undo
//!   ([`Store::undo_batch`]) shows the draft again.
//!
//! Writes are ordered without clocks. Each draft has a `lifecycle`, bumped when it's sent and
//! when Undo send brings it back. The app learns it with the draft, and each write carries the
//! lifecycle it was made in ([`DraftRev`]), so a write made before a send can't change what was
//! sent, or the draft Undo send gave back. Within a lifecycle the app's stamps (`rev`) order
//! saves. Once a draft is gone for good (sent, purged, discarded), `draft_ends` keeps its id:
//! no write from its life brings it back. Such a write is refused, never taken for saved: the
//! app keeps its text as a new draft. Only a send (which comes after everything) starts it again.
//!
//! `version` counts content changes and `server_version` is the one the server has, so an
//! upload knows whether there's anything to do. `appended` says an upload was started: the
//! server may have a copy, and deleting the draft must look there. `server_mailbox`,
//! `server_validity` and `server_uid` say where the last confirmed copy is, so the next save
//! deletes exactly that one; `unconfirmed` marks an upload that didn't confirm (cut off, or its
//! cleanup unsure), so the next one also searches for strays.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Op, Store, json};
use crate::model::{Draft, DraftAttachment, FileSource, FileStatus, MessageId};

/// Where a write to a draft stands: the draft's lifecycle it was made in, and the writer's
/// stamp, which orders saves within it ([`Store::save_draft`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DraftRev {
    pub lifecycle: i64,
    pub rev: i64,
}

/// A local draft.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalDraft {
    pub id: i64,
    pub account: String,
    pub draft: Draft,
    pub message_id: String,
    /// Unix seconds of the last change.
    pub updated: i64,
    /// Neither being sent nor discarded.
    pub visible: bool,
    /// Its lifecycle, for writes to carry, and the newest save's stamp (the app's stamps go on
    /// from above it, whatever its clock says).
    pub at: DraftRev,
}

/// A draft version for the worker to upload.
#[derive(Debug, PartialEq)]
pub struct DraftUpload {
    pub account: String,
    pub draft: Draft,
    pub message_id: String,
    pub version: i64,
    /// The round of uploads it belongs to: its completion counts only if the draft is still
    /// in that round ([`Store::draft_uploaded`]).
    pub generation: i64,
    /// The last confirmed copy, `(mailbox, UIDVALIDITY, UID)`, which this upload replaces.
    pub previous: Option<(String, u32, u32)>,
    /// An earlier upload didn't confirm, and may have left a copy behind.
    pub maybe_strays: bool,
}

const COLS: &str = "d.id, d.account, d.content, d.message_id, d.updated, d.sending = 0 AND d.purge_op = 0, d.lifecycle, d.rev";

/// A draft's files: their columns in draft_attachments, and what `status` holds.
const FILE_COLS: &str = "id, filename, mime, size, status, error";
const ATTACHED: i64 = 0;
const ATTACHING: i64 = 1;
const FAILED: i64 = 2;
/// Taken off: kept only so its id is never given again.
const REMOVED: i64 = 3;

fn file_row(r: &rusqlite::Row) -> rusqlite::Result<DraftAttachment> {
    let status = match r.get::<_, i64>(4)? {
        ATTACHING => FileStatus::Attaching,
        FAILED => FileStatus::Failed(r.get(5)?),
        _ => FileStatus::Attached,
    };
    Ok(DraftAttachment { id: r.get(0)?, filename: r.get(1)?, mime: r.get(2)?, size: r.get::<_, i64>(3)? as usize, status })
}

/// What's kept of a draft's content: its text, not its files, which are the store's.
fn text_of(draft: &Draft) -> String {
    if draft.attachments.is_empty() { json(draft) } else { json(&Draft { attachments: Vec::new(), ..draft.clone() }) }
}

/// Runs for every Drafts list: it must read only the Drafts mailboxes, never a whole account
/// (messages has no index on message_id). CROSS JOIN fixes the order: Drafts mailboxes, then
/// their messages by (account, mailbox), then the few drafts.
const DRAFT_COPIES: &str = "SELECT m.id, d.id FROM mailboxes b
     CROSS JOIN messages m ON m.account = b.account AND m.mailbox = b.name
     CROSS JOIN drafts d ON d.account = m.account AND d.message_id = m.message_id
     WHERE b.role = 'drafts' AND (?1 IS NULL OR b.account = ?1)";

fn row(r: &rusqlite::Row) -> rusqlite::Result<LocalDraft> {
    let account: String = r.get(1)?;
    let mut draft: Draft = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
    draft.account = account.clone();
    let at = DraftRev { lifecycle: r.get(6)?, rev: r.get(7)? };
    Ok(LocalDraft { id: r.get(0)?, account, draft, message_id: r.get(3)?, updated: r.get(4)?, visible: r.get(5)?, at })
}

/// A Message-ID for a new draft. The angle brackets make a search for it (a substring match in
/// IMAP) find this draft's copies and nothing else.
fn new_message_id(id: i64) -> String {
    format!("<{id}.{:08x}.draft@envoy>", rand::random::<u32>())
}

impl Store {
    /// Saves the composer's content as draft `id`, creating it on the first save. `at` orders
    /// it. A save from another lifecycle of the draft (made before it was sent, or before Undo
    /// send gave it back), or for a draft gone for good, is refused: None, it isn't saved, and
    /// the caller must keep its content some other way. A save no newer than the last one
    /// applied, such as a queued save overtaken by the one made as the window closed, changes
    /// nothing. `open`: it's in the composer, to be opened again after a restart.
    ///
    /// Saving shows a draft whose discard is on its way to the server (a too-late undo put it
    /// back in the composer; [`Store::draft_purged`] keeps it). Moving a draft to another
    /// account queues the deletion of its copies on the old account's server. Returns whether
    /// the content changed.
    pub fn save_draft(&self, id: i64, draft: &Draft, open: bool, at: DraftRev, now: i64) -> Result<Option<bool>> {
        self.save_draft_from(id, draft, open, at, now, None)
    }

    /// [`Store::save_draft`] for a draft that carries on another's text (`files_from`: a save
    /// refused under its own id, kept as a new draft): it gets that draft's files, those still
    /// being attached too (whose bytes, once read, go to both).
    pub fn save_draft_from(&self, id: i64, draft: &Draft, open: bool, at: DraftRev, now: i64, files_from: Option<i64>) -> Result<Option<bool>> {
        self.transaction(|st| {
            if let Some(from) = files_from.filter(|f| *f != id) {
                st.copy_draft_files(from, id, &draft.attachments)?;
            }
            let saved = st.put_draft(id, draft, open, Some(at.lifecycle), Some(at.rev), now)?;
            if saved.is_none() {
                // Refused: what was copied for it goes too.
                st.drop_files_without_draft(id)?;
            }
            Ok(saved)
        })
    }

    /// [`Store::save_draft`]. `lifecycle: None` writes in whatever lifecycle: the send itself,
    /// which comes after everything (it starts a draft gone for good again). `rev: None` writes
    /// whatever the stored rev. The draft's files are the store's (`draft_attachments`): a
    /// write's list is ignored, so no save, however late, adds or takes off a file. Call inside
    /// a transaction.
    fn put_draft(&self, id: i64, draft: &Draft, open: bool, lifecycle: Option<i64>, rev: Option<i64>, now: i64) -> Result<Option<bool>> {
        // Gone for good: no write from its life brings it back.
        let ended: Option<i64> = self.conn.query_row("SELECT lifecycle FROM draft_ends WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        if ended.is_some() {
            if lifecycle.is_some() {
                return Ok(None);
            }
            self.conn.execute("DELETE FROM draft_ends WHERE id = ?1", params![id])?;
        }
        let content = text_of(draft);
        type Row = (String, String, String, i64, bool, Option<(String, u32, u32)>, bool, bool, i64, i64);
        let old: Option<Row> = self
            .conn
            .query_row(
                "SELECT account, content, message_id, rev, appended != 0, server_mailbox, server_validity, server_uid, server_account = account, unconfirmed != 0,
                     sending != 0 OR sent != 0, upload_gen, lifecycle
                 FROM drafts WHERE id = ?1",
                params![id],
                |r| {
                    let copy = (r.get::<_, u32>(7)? != 0 && r.get::<_, bool>(8)?).then(|| Ok::<_, rusqlite::Error>((r.get(5)?, r.get(6)?, r.get(7)?))).transpose()?;
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, copy, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?))
                },
            )
            .optional()?;
        let Some((account, old_content, message_id, old_rev, appended, copy, unconfirmed, outbox, generation, old_lifecycle)) = old else {
            // Started again by a send: a lifecycle after every earlier one.
            let lifecycle = lifecycle.or(ended.map(|l| l + 1)).unwrap_or(0);
            self.conn.execute(
                "INSERT INTO drafts (id, account, content, message_id, rev, updated, version, open, lifecycle) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
                params![id, draft.account, content, new_message_id(id), rev.unwrap_or(0), now, open, lifecycle],
            )?;
            return Ok(Some(true));
        };
        // From another lifecycle: refused. So is any write to a draft in the outbox (or sent):
        // it's what was sent, for Undo send to give back.
        if lifecycle.is_some_and(|l| l != old_lifecycle) || (outbox && lifecycle.is_some()) {
            return Ok(None);
        }
        if rev.is_some_and(|r| r <= old_rev) {
            return Ok(Some(false));
        }
        let moved = account != draft.account;
        if moved && appended {
            // Its copies on the old account's server go, there: the one on record by UID, and
            // by Message-ID only if an upload may have left one no one knows the UID of.
            let exact = copy.is_some() && !unconfirmed;
            self.enqueue(&account, &Op::PurgeDraft { message_id, draft: None, copy, exact, left_by: Some((id, generation)) })?;
        }
        let changed = old_content != content;
        // Moved: a new round of uploads, on the new account; one still running for the old
        // account can't touch it ([`Store::draft_uploaded`]).
        self.conn.execute(
            "UPDATE drafts SET account = ?2, content = ?3, rev = ?4, open = ?5, purge_op = CASE WHEN sent THEN purge_op ELSE 0 END,
                 updated = CASE WHEN ?6 THEN ?7 ELSE updated END, version = version + ?6,
                 appended = appended AND NOT ?8, server_version = CASE WHEN ?8 THEN 0 ELSE server_version END,
                 server_uid = CASE WHEN ?8 THEN 0 ELSE server_uid END, unconfirmed = unconfirmed AND NOT ?8,
                 upload_gen = upload_gen + ?8
             WHERE id = ?1",
            params![id, draft.account, content, rev.unwrap_or(old_rev), open, changed, now, moved],
        )?;
        Ok(Some(changed))
    }

    /// A draft as read, with its files.
    fn with_files(&self, mut d: LocalDraft) -> Result<LocalDraft> {
        d.draft.attachments = self.files_of(d.id)?;
        Ok(d)
    }

    /// Any draft, shown or not.
    pub fn draft(&self, id: i64) -> Result<Option<LocalDraft>> {
        let d = self.conn.query_row(&format!("SELECT {COLS} FROM drafts d WHERE d.id = ?1"), params![id], row).optional()?;
        d.map(|d| self.with_files(d)).transpose()
    }

    /// Every draft, of one account or all, newest first, shown or not: what the Drafts list is
    /// merged with.
    pub fn drafts(&self, account: Option<&str>) -> Result<Vec<LocalDraft>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLS} FROM drafts d WHERE ?1 IS NULL OR d.account = ?1 ORDER BY d.updated DESC, d.id DESC"))?;
        let rows: Vec<LocalDraft> = stmt.query_map(params![account], row)?.collect::<rusqlite::Result<_>>()?;
        rows.into_iter().map(|d| self.with_files(d)).collect()
    }

    /// The draft that was in the composer when the app last closed, to open again.
    pub fn draft_to_restore(&self) -> Result<Option<LocalDraft>> {
        let sql = format!("SELECT {COLS} FROM drafts d WHERE d.open = 1 AND d.sending = 0 AND d.purge_op = 0 ORDER BY d.updated DESC, d.id DESC LIMIT 1");
        let d = self.conn.query_row(&sql, [], row).optional()?;
        d.map(|d| self.with_files(d)).transpose()
    }

    pub fn set_draft_open(&self, id: i64, open: bool) -> Result<()> {
        self.conn.execute("UPDATE drafts SET open = ?2 WHERE id = ?1", params![id, open])?;
        Ok(())
    }

    /// Cached messages in Drafts mailboxes that are server copies of local drafts, of one
    /// account or all: `(message id, draft id)`.
    pub fn draft_copies(&self, account: Option<&str>) -> Result<Vec<(MessageId, i64)>> {
        let mut stmt = self.conn.prepare_cached(DRAFT_COPIES)?;
        let rows = stmt.query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The local draft this cached message is a server copy of.
    pub fn draft_of_message(&self, id: MessageId) -> Result<Option<LocalDraft>> {
        let sql = format!(
            "SELECT {COLS} FROM drafts d JOIN messages m ON m.account = d.account AND m.message_id = d.message_id WHERE m.id = ?1 AND m.message_id != ''"
        );
        let d = self.conn.query_row(&sql, params![id], row).optional()?;
        d.map(|d| self.with_files(d)).transpose()
    }

    /// Makes a draft found on the server (written elsewhere, or by Envoy on another computer) a
    /// local draft, so it can be edited, with its attachments' bytes, `files` (in the order of
    /// `draft.attachments`). It keeps the copy's Message-ID, so its first save replaces that
    /// copy. A copy without one gets a Message-ID of its own: it's uploaded again at once, and
    /// the old copy is deleted (hidden meanwhile). None if the message is gone.
    pub fn adopt_draft(&self, message: MessageId, draft: &Draft, files: &[Vec<u8>], now: i64) -> Result<Option<LocalDraft>> {
        self.transaction(|st| {
            let Some((account, mailbox, uid)) = st.location(message)? else { return Ok(None) };
            let message_id: String = st.conn.query_row("SELECT message_id FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
            let id = st.fresh_draft_id(now)?;
            let draft = Draft { account: account.clone(), ..draft.clone() };
            let fresh = message_id.is_empty();
            let message_id = if fresh { new_message_id(id) } else { message_id };
            st.conn.execute(
                "INSERT INTO drafts (id, account, content, message_id, updated, version, server_version, appended, open) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6, 1)",
                params![id, account, text_of(&draft), message_id, now, if fresh { 0 } else { 1 }],
            )?;
            for (a, data) in draft.attachments.iter().zip(files) {
                st.conn.execute("INSERT INTO draft_attachments (draft, id, filename, mime, size) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, a.id, a.filename, a.mime, data.len()])?;
                st.conn.execute("INSERT INTO draft_files (draft, id, data) VALUES (?1, ?2, ?3)", params![id, a.id, data])?;
            }
            if fresh {
                let op = st.enqueue(&account, &Op::Expunge { mailbox, uid })?;
                st.hide_local(message, op)?;
                st.queue_draft_upload(&account, id, now)?;
            }
            st.draft(id)
        })
    }

    /// An id for a draft the engine starts (the app picks its own): used by no draft, here or
    /// gone, and like the app's ids, microseconds since the epoch where the clock allows.
    pub fn fresh_draft_id(&self, now: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT MAX(COALESCE((SELECT MAX(id) FROM drafts), 0) + 1, COALESCE((SELECT MAX(id) FROM draft_ends), 0) + 1, ?1)",
            params![now.saturating_mul(1_000_000)],
            |r| r.get(0),
        )?)
    }

    /// Saves `draft` as a new draft, `rev` its stamp, under an id no draft has had, picked and
    /// taken in one statement (it holds the write lock from the start): drafts made at once,
    /// by other Envoys too, can't land on the same id. Returns the id.
    pub fn insert_new_draft(&self, draft: &Draft, open: bool, rev: i64, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO drafts (id, account, content, message_id, rev, updated, version, open)
             SELECT n, ?1, ?2, '<' || n || '.' || ?3 || '.draft@envoy>', ?4, ?5, 1, ?6
             FROM (SELECT MAX(COALESCE((SELECT MAX(id) FROM drafts), 0) + 1, COALESCE((SELECT MAX(id) FROM draft_ends), 0) + 1, ?7) AS n)",
            params![draft.account, text_of(draft), format!("{:08x}", rand::random::<u32>()), rev, now, open, now.saturating_mul(1_000_000)],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The draft's files, in the order they were added (their ids), each with where it stands.
    pub fn files_of(&self, id: i64) -> Result<Vec<DraftAttachment>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {FILE_COLS} FROM draft_attachments WHERE draft = ?1 AND status != {REMOVED} ORDER BY id"))?;
        let rows = stmt.query_map(params![id], file_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The draft's attached files with their bytes, in order: what's uploaded and sent with it.
    /// An error if one's bytes are missing: a draft never goes without a file it lists.
    pub fn attached_files(&self, id: i64) -> Result<Vec<(DraftAttachment, Vec<u8>)>> {
        let mut stmt = self.conn.prepare_cached("SELECT data FROM draft_files WHERE draft = ?1 AND id = ?2")?;
        self.files_of(id)?
            .into_iter()
            .filter(|a| a.status == FileStatus::Attached)
            .map(|a| {
                let data: Option<Vec<u8>> = stmt.query_row(params![id, a.id], |r| r.get(0)).optional()?;
                data.map(|d| (a.clone(), d)).ok_or_else(|| anyhow::anyhow!("The attached file {} is missing from this draft", a.filename))
            })
            .collect()
    }

    /// Starts attaching `files` to draft `id`: each is listed from now on, as being attached,
    /// with where it comes from, until its bytes are in ([`Store::file_in`]) or it can't be
    /// ([`Store::file_failed`]), whoever is (or isn't) looking at the draft meanwhile. Only for
    /// a draft the store has (a composer saves before it attaches): None, and nothing listed,
    /// when it's gone, so no file outlives its draft. An id the draft already has, or had, is
    /// refused, never taken over: false for that file.
    pub fn attaching(&self, id: i64, files: &[(DraftAttachment, FileSource)]) -> Result<Option<Vec<bool>>> {
        self.transaction(|st| {
            if st.conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM drafts WHERE id = ?1)", params![id], |r| r.get::<_, bool>(0))? {
                return Ok(None);
            }
            let mut stmt = st.conn.prepare_cached(&format!(
                "INSERT OR IGNORE INTO draft_attachments (draft, id, filename, mime, size, status, source) VALUES (?1, ?2, ?3, ?4, ?5, {ATTACHING}, ?6)"
            ))?;
            let taken = files.iter().map(|(a, from)| Ok(stmt.execute(params![id, a.id, a.filename, a.mime, a.size, json(from)])? > 0)).collect::<Result<_>>()?;
            Ok(Some(taken))
        })
    }

    /// File `file` is read: its bytes go with every draft attaching it (a draft carrying on
    /// another's text attaches the same file), written once, and each draft gets an upload.
    /// Returns those drafts, with the file as it now is. A file no longer being attached (taken
    /// off, or its draft gone) is left alone: an answer that comes late changes nothing.
    pub fn file_in(&self, file: i64, data: &[u8], mime: &str, now: i64) -> Result<Vec<(i64, DraftAttachment)>> {
        self.transaction(|st| {
            let drafts: Vec<i64> = st
                .conn
                .prepare_cached(&format!("SELECT draft FROM draft_attachments WHERE id = ?1 AND status = {ATTACHING}"))?
                .query_map(params![file], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut done = Vec::new();
            for draft in drafts {
                // Bytes are never replaced: a draft's file id names one file, ever.
                if st.conn.execute("INSERT OR IGNORE INTO draft_files (draft, id, data) VALUES (?1, ?2, ?3)", params![draft, file, data])? == 0 {
                    st.conn.execute(&format!("UPDATE draft_attachments SET status = {FAILED}, error = 'Couldn''t attach it: try again' WHERE draft = ?1 AND id = ?2"), params![draft, file])?;
                    continue;
                }
                st.conn.execute(
                    &format!("UPDATE draft_attachments SET status = {ATTACHED}, size = ?3, mime = CASE WHEN ?4 = '' THEN mime ELSE ?4 END, error = '' WHERE draft = ?1 AND id = ?2"),
                    params![draft, file, data.len(), mime],
                )?;
                st.files_changed(draft, now)?;
                if let Some(a) = st.files_of(draft)?.into_iter().find(|a| a.id == file) {
                    done.push((draft, a));
                }
            }
            Ok(done)
        })
    }

    /// File `file` couldn't be attached (`error`, to show): it stays listed, saying so, for the
    /// user to take off or attach again. Returns the drafts that were attaching it.
    pub fn file_failed(&self, file: i64, error: &str) -> Result<Vec<i64>> {
        self.transaction(|st| {
            let drafts: Vec<i64> = st
                .conn
                .prepare_cached(&format!("SELECT draft FROM draft_attachments WHERE id = ?1 AND status = {ATTACHING}"))?
                .query_map(params![file], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            st.conn.execute(&format!("UPDATE draft_attachments SET status = {FAILED}, error = ?2 WHERE id = ?1 AND status = {ATTACHING}"), params![file, error])?;
            Ok(drafts)
        })
    }

    /// Takes file `file` off draft `id`, its bytes too. Its id stays the draft's, never given
    /// to another file, so an answer still on its way for it changes nothing. True if it was
    /// attached (the draft changed, and gets an upload).
    pub fn remove_file(&self, id: i64, file: i64, now: i64) -> Result<bool> {
        self.transaction(|st| {
            let status: Option<i64> = st.conn.query_row("SELECT status FROM draft_attachments WHERE draft = ?1 AND id = ?2", params![id, file], |r| r.get(0)).optional()?;
            if status.is_none() {
                // Not listed yet (taken off on close, its attach still queued): its tombstone now,
                // so the attach, when it comes, finds the id taken.
                st.conn.execute(&format!("INSERT INTO draft_attachments (draft, id, filename, mime, status) SELECT ?1, ?2, '', '', {REMOVED} WHERE EXISTS (SELECT 1 FROM drafts WHERE id = ?1)"), params![id, file])?;
            }
            if status.is_none_or(|s| s == REMOVED) {
                return Ok(false);
            }
            st.conn.execute(&format!("UPDATE draft_attachments SET status = {REMOVED}, error = '' WHERE draft = ?1 AND id = ?2"), params![id, file])?;
            st.conn.execute("DELETE FROM draft_files WHERE draft = ?1 AND id = ?2", params![id, file])?;
            let attached = status == Some(ATTACHED);
            if attached {
                st.files_changed(id, now)?;
            }
            Ok(attached)
        })
    }

    /// Files still being attached, as a restart finds them: `(draft, file, where it comes
    /// from)`, once per file (with one of the drafts attaching it).
    pub fn attaching_files(&self) -> Result<Vec<(i64, i64, FileSource)>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT MIN(draft), id, MIN(source) FROM draft_attachments WHERE status = {ATTACHING} GROUP BY id ORDER BY id"))?;
        let rows: Vec<(i64, i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().filter_map(|(draft, id, source)| serde_json::from_str(&source).ok().map(|s| (draft, id, s))).collect())
    }

    /// How much the draft's attached files weigh.
    pub fn attached_size(&self, id: i64) -> Result<usize> {
        Ok(self.conn.query_row(&format!("SELECT COALESCE(SUM(size), 0) FROM draft_attachments WHERE draft = ?1 AND status = {ATTACHED}"), params![id], |r| r.get::<_, i64>(0))? as usize)
    }

    /// The draft's files changed: its server copy is behind, and gets an upload (in a while if
    /// the draft is being written, and longer the more its files weigh; now if it's put away).
    /// Nothing for a draft on its way out. Call inside a transaction.
    fn files_changed(&self, id: i64, now: i64) -> Result<()> {
        let found: Option<(String, bool)> = self
            .conn
            .query_row("SELECT account, open != 0 FROM drafts WHERE id = ?1 AND sending = 0 AND purge_op = 0", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((account, open)) = found else { return Ok(()) };
        self.conn.execute("UPDATE drafts SET version = version + 1, updated = ?2 WHERE id = ?1", params![id, now])?;
        let at = if open { now + crate::drafts::upload_delay(self.attached_size(id)?) } else { now };
        self.queue_draft_upload(&account, id, at)?;
        Ok(())
    }

    /// Copies draft `from`'s files to draft `id`, leaving any `id` has, or had. The files the
    /// composer shows attached (`shown`) that `from` no longer has (they went with it) are
    /// kept, as not attached, saying why. (Files it shows on their way may be ones it's about to
    /// attach to `id`: their ids are left to them.) Call inside a transaction that saves draft
    /// `id`.
    fn copy_draft_files(&self, from: i64, id: i64, shown: &[DraftAttachment]) -> Result<()> {
        self.conn.execute(
            &format!("INSERT OR IGNORE INTO draft_attachments (draft, id, filename, mime, size, status, error, source) SELECT ?2, id, filename, mime, size, status, error, source FROM draft_attachments WHERE draft = ?1 AND status != {REMOVED}"),
            params![from, id],
        )?;
        // Bytes only for files the draft has, attached: never behind one it took off.
        self.conn.execute(
            &format!("INSERT OR IGNORE INTO draft_files (draft, id, data) SELECT ?2, f.id, f.data FROM draft_files f JOIN draft_attachments a ON a.draft = ?2 AND a.id = f.id AND a.status = {ATTACHED} WHERE f.draft = ?1"),
            params![from, id],
        )?;
        let mut lost = self.conn.prepare_cached(&format!(
            "INSERT OR IGNORE INTO draft_attachments (draft, id, filename, mime, size, status, error) VALUES (?1, ?2, ?3, ?4, ?5, {FAILED}, 'Lost with the draft it was attached to: attach it again')"
        ))?;
        for f in shown.iter().filter(|f| f.status == FileStatus::Attached) {
            lost.execute(params![id, f.id, f.filename, f.mime, f.size])?;
        }
        Ok(())
    }

    /// Files copied for draft `id` that never got their draft (its save was refused). Call
    /// inside a transaction.
    fn drop_files_without_draft(&self, id: i64) -> Result<()> {
        for table in ["draft_files", "draft_attachments"] {
            self.conn.execute(&format!("DELETE FROM {table} WHERE draft = ?1 AND NOT EXISTS (SELECT 1 FROM drafts WHERE id = ?1)"), params![id])?;
        }
        Ok(())
    }

    /// A new draft that carries on another's text ([`Store::insert_new_draft`]) gets the files
    /// of draft `from` (those `shown` that it lost, as not attached). Call inside a
    /// transaction.
    pub fn take_files_from(&self, from: Option<i64>, id: i64, shown: &[DraftAttachment]) -> Result<()> {
        match from.filter(|f| *f != id) {
            Some(from) => self.copy_draft_files(from, id, shown),
            None => Ok(()),
        }
    }

    /// Whether the draft has changes the server's copy lacks, and should get them (it isn't on
    /// its way out).
    pub fn draft_needs_upload(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT version > server_version FROM drafts WHERE id = ?1 AND sending = 0 AND purge_op = 0", params![id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// Queues uploading a draft, unless an upload of it is already waiting: that one reads the
    /// newest version when it runs, so rapid edits make one server update. A waiting upload is
    /// brought forward to `not_before` if that's sooner. True when the queue changed (the worker
    /// should plan its wait again).
    pub fn queue_draft_upload(&self, account: &str, id: i64, not_before: i64) -> Result<bool> {
        let op = Op::SaveDraft { draft: id };
        let waiting: Option<(i64, i64)> = self
            .conn
            .query_row("SELECT id, not_before FROM ops WHERE account = ?1 AND op = ?2 AND claimed = 0", params![account, json(&op)], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match waiting {
            Some((_, at)) if at <= not_before => Ok(false),
            Some((waiting, _)) => {
                self.conn.execute("UPDATE ops SET not_before = ?2 WHERE id = ?1", params![waiting, not_before])?;
                Ok(true)
            }
            None => {
                self.enqueue_at(account, &op, not_before)?;
                Ok(true)
            }
        }
    }

    /// Upload op `op` failed and is back in the queue: another upload of the same draft waiting
    /// too (queued by an edit made while `op` was on the wire) is folded into it, due as soon
    /// as either was. Uploads read the newest version, so the edit isn't lost or held back, and
    /// retries can't pile up. Call inside a transaction.
    pub(super) fn coalesce_uploads(&self, op: i64) -> Result<()> {
        let found: Option<(String, String)> = self.conn.query_row("SELECT account, op FROM ops WHERE id = ?1", params![op], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((account, kind)) = found else { return Ok(()) };
        if !matches!(serde_json::from_str(&kind), Ok(Op::SaveDraft { .. })) {
            return Ok(());
        }
        let others: Option<i64> = self.conn.query_row(
            "SELECT MIN(not_before) FROM ops WHERE account = ?1 AND op = ?2 AND id != ?3 AND claimed = 0",
            params![account, kind, op],
            |r| r.get(0),
        )?;
        if let Some(due) = others {
            self.conn.execute("DELETE FROM ops WHERE account = ?1 AND op = ?2 AND id != ?3 AND claimed = 0", params![account, kind, op])?;
            self.conn.execute("UPDATE ops SET not_before = MIN(not_before, ?2) WHERE id = ?1", params![op, due])?;
        }
        Ok(())
    }

    /// The draft's newest version, for the worker to upload to `account`'s Drafts mailbox, if
    /// the server's copy is behind and the draft is staying (not being sent or discarded, and
    /// still on that account). From here on the server may have a copy, even if the upload is
    /// cut off.
    pub fn draft_upload(&self, account: &str, id: i64) -> Result<Option<DraftUpload>> {
        type Row = (String, String, i64, i64, Option<(String, u32, u32)>, bool);
        let found: Option<Row> = self
            .conn
            .query_row(
                "SELECT content, message_id, version, upload_gen, server_mailbox, server_validity, server_uid, server_account, unconfirmed != 0 FROM drafts
                 WHERE id = ?1 AND account = ?2 AND sending = 0 AND purge_op = 0 AND version > server_version",
                params![id, account],
                |r| {
                    // Only a copy on this account's server: a UID means nothing anywhere else.
                    let mine = r.get::<_, u32>(6)? != 0 && r.get::<_, String>(7)? == account;
                    let previous = mine.then(|| Ok::<_, rusqlite::Error>((r.get(4)?, r.get(5)?, r.get(6)?))).transpose()?;
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, previous, r.get(8)?))
                },
            )
            .optional()?;
        let Some((content, message_id, version, generation, previous, maybe_strays)) = found else { return Ok(None) };
        self.conn.execute("UPDATE drafts SET appended = 1, unconfirmed = 1 WHERE id = ?1", params![id])?;
        let mut draft: Draft = serde_json::from_str(&content).unwrap_or_default();
        draft.account = account.to_owned();
        draft.attachments = self.files_of(id)?.into_iter().filter(|a| a.status == FileStatus::Attached).collect();
        Ok(Some(DraftUpload { account: account.to_owned(), draft, message_id, version, generation, previous, maybe_strays }))
    }

    /// Upload `up` finished: the server has its version, as `copy` (`(UIDVALIDITY, UID)` in
    /// `mailbox`) if the UID is known; `certain`: the copies before it are known to be gone.
    ///
    /// Recorded only if the draft is still on that account, in that round of uploads. An upload
    /// that finished after the draft moved to another account (or was discarded and started
    /// over) left a copy the draft no longer owns: its deletion is queued on the account it's
    /// on, by UID, and false is returned.
    pub fn draft_uploaded(&self, id: i64, up: &DraftUpload, mailbox: &str, copy: Option<(u32, u32)>, certain: bool) -> Result<bool> {
        self.transaction(|st| {
            let (validity, uid) = copy.unwrap_or((0, 0));
            let recorded = st.conn.execute(
                "UPDATE drafts SET server_version = MAX(server_version, ?4), server_account = ?2, server_mailbox = ?5, server_validity = ?6, server_uid = ?7, unconfirmed = ?8
                 WHERE id = ?1 AND account = ?2 AND upload_gen = ?3",
                params![id, up.account, up.generation, up.version, mailbox, validity, uid, !certain],
            )? > 0;
            if !recorded && st.draft(id)?.is_some() {
                let copy = copy.map(|(v, u)| (mailbox.to_owned(), v, u));
                let exact = copy.is_some();
                st.enqueue(&up.account, &Op::PurgeDraft { message_id: up.message_id.clone(), draft: None, copy, exact, left_by: Some((id, up.generation)) })?;
            }
            Ok(recorded)
        })
    }

    /// The draft's server copy on record: `(account, mailbox, UIDVALIDITY, UID)`.
    pub fn draft_copy(&self, id: i64) -> Result<Option<(String, String, u32, u32)>> {
        Ok(self
            .conn
            .query_row("SELECT server_account, server_mailbox, server_validity, server_uid FROM drafts WHERE id = ?1 AND server_uid != 0", params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?)
    }

    /// A cleanup of the copies draft `id` left on `account` in upload generation `generation`
    /// ([`Op::PurgeDraft`] without a draft) searched by Message-ID. If the draft is back on that
    /// account in a newer generation, with an upload started, its new copy may be gone (on this
    /// try or an earlier one whose answer was lost): the draft is marked as behind the server,
    /// and its upload queued (looking for strays, in case the copy is still there). Returns
    /// whether it was.
    pub fn draft_cleaned_up(&self, id: i64, account: &str, generation: i64, now: i64) -> Result<bool> {
        self.transaction(|st| {
            let lost = st.conn.execute(
                "UPDATE drafts SET server_version = 0, unconfirmed = 1 WHERE id = ?1 AND account = ?2 AND upload_gen > ?3 AND appended != 0",
                params![id, account, generation],
            )? > 0;
            if lost && st.draft_needs_upload(id)? {
                st.queue_draft_upload(account, id, now)?;
            }
            Ok(lost)
        })
    }

    /// Whether the server may have copies of the draft (an upload was started). False for a
    /// draft that's gone.
    pub fn draft_appended(&self, id: i64) -> Result<bool> {
        Ok(self.conn.query_row("SELECT appended != 0 FROM drafts WHERE id = ?1", params![id], |r| r.get(0)).optional()?.unwrap_or(false))
    }

    /// The draft's server copies are gone ([`Op::PurgeDraft`]), and so is the draft, unless it
    /// was saved since (back in the composer after a too-late undo): then it starts over, and is
    /// uploaded again.
    pub fn draft_purged(&self, id: i64, now: i64) -> Result<()> {
        self.transaction(|st| {
            if st.conn.execute("DELETE FROM drafts WHERE id = ?1 AND purge_op != 0", params![id])? > 0 {
                return Ok(());
            }
            let account: Option<String> = st.conn.query_row("SELECT account FROM drafts WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
            let Some(account) = account else { return Ok(()) };
            st.conn.execute(
                "UPDATE drafts SET appended = 0, server_version = 0, server_uid = 0, unconfirmed = 0, upload_gen = upload_gen + 1 WHERE id = ?1",
                params![id],
            )?;
            if st.draft_needs_upload(id)? {
                st.queue_draft_upload(&account, id, now)?;
            }
            Ok(())
        })
    }

    /// Puts a draft in the outbox, in send op `op`, with the content being sent (so undoing the
    /// send gives exactly that back): hidden until the send goes out ([`Store::draft_sent`]) or
    /// is undone ([`Store::unsend_draft`]). Creates the draft if the composer never saved it.
    /// Call inside a transaction.
    pub fn draft_sending(&self, id: i64, draft: &Draft, op: i64, now: i64) -> Result<()> {
        // Being sent now, after whatever came before; in a lifecycle of its own, so nothing
        // written before it can change what was sent.
        self.put_draft(id, draft, false, None, None, now)?;
        self.conn.execute("UPDATE drafts SET sending = ?2, lifecycle = lifecycle + 1 WHERE id = ?1", params![id, op])?;
        Ok(())
    }

    /// Keeps a message that wasn't sent as a draft, in the composer, with what was being sent:
    /// its send couldn't be queued, or the window closed before the engine confirmed it. `at`:
    /// the composer's when it sent, ordering it like a save (it overwrites nothing newer); None
    /// for the engine's own copy, written as the send failed, which nothing came after. True
    /// when that's dealt with: kept, or overtaken, or its send is queued or went out (nothing
    /// to keep). False when refused like a save ([`Store::save_draft`]): the draft ended
    /// without being sent, or moved on, and the caller must keep the text.
    pub fn draft_unsent(&self, id: i64, draft: &Draft, at: Option<DraftRev>, now: i64) -> Result<bool> {
        self.transaction(|st| {
            if st.draft_on_its_way(id)? {
                return Ok(true);
            }
            Ok(st.put_draft(id, draft, true, at.map(|a| a.lifecycle), at.map(|a| a.rev), now)?.is_some())
        })
    }

    /// Whether the draft's send is queued, or went out.
    pub fn draft_on_its_way(&self, id: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM drafts WHERE id = ?1 AND (sending != 0 OR sent != 0)) OR EXISTS (SELECT 1 FROM draft_ends WHERE id = ?1 AND sent != 0)",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// Brings back the draft whose send (op `op`) was undone, in the composer, queuing the
    /// upload it skipped while it waited. It's in a new lifecycle (the composer gets it with
    /// the draft): nothing written before the send can touch it. Call inside a transaction,
    /// with the cancel.
    pub fn unsend_draft(&self, op: i64, now: i64) -> Result<Option<LocalDraft>> {
        let id: Option<i64> = self.conn.query_row("SELECT id FROM drafts WHERE sending = ?1 AND sending != 0", params![op], |r| r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        self.conn.execute("UPDATE drafts SET sending = 0, open = 1, lifecycle = lifecycle + 1 WHERE id = ?1", params![id])?;
        let back = self.draft(id)?;
        if let Some(d) = &back
            && self.draft_needs_upload(id)?
        {
            self.queue_draft_upload(&d.account, id, now)?;
        }
        Ok(back)
    }

    /// Send op `op` can never go (its server won't take a message that big): it's off the
    /// queue, and its draft is back in the composer, as Undo send gives it back.
    pub fn send_bounced(&self, op: i64, now: i64) -> Result<Option<LocalDraft>> {
        self.transaction(|st| {
            st.conn.execute("DELETE FROM ops WHERE id = ?1", params![op])?;
            st.unsend_draft(op, now)
        })
    }

    /// Send op `op` went out: the drafts it carried are done with. One that may have copies on
    /// the server stays hidden until its [`Op::PurgeDraft`] deletes them; one that never got
    /// there goes now. Call inside a transaction.
    pub(super) fn draft_sent(&self, op: i64) -> Result<()> {
        let sent: Vec<(i64, String, String, bool)> = self
            .conn
            .prepare_cached("SELECT id, account, message_id, appended != 0 FROM drafts WHERE sending = ?1 AND sending != 0")?
            .query_map(params![op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, account, message_id, appended) in sent {
            if appended {
                let purge = self.enqueue(&account, &Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false, left_by: None })?;
                self.conn.execute("UPDATE drafts SET sending = 0, purge_op = ?2, open = 0, sent = 1 WHERE id = ?1", params![id, purge])?;
            } else {
                self.conn.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
            }
        }
        Ok(())
    }

    /// Discards a draft: hidden at once; its server copies and then the draft go when its
    /// [`Op::PurgeDraft`] replays, after the undo window with `hold = Some((not_before, key))`,
    /// during which [`Store::undo_batch`] takes it back. Without a hold, a draft that never
    /// reached the server goes at once. Returns its account, or None when there's no such draft
    /// to discard (gone, or already on its way out).
    pub fn discard_draft(&self, id: i64, hold: Option<(i64, i64)>) -> Result<Option<String>> {
        self.transaction(|st| {
            let found: Option<(String, String, bool)> = st
                .conn
                .query_row("SELECT account, message_id, appended != 0 FROM drafts WHERE id = ?1 AND sending = 0 AND purge_op = 0", params![id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((account, message_id, appended)) = found else { return Ok(None) };
            let op = Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false, left_by: None };
            let purge = match hold {
                Some((not_before, key)) => st.enqueue_held(&account, &op, not_before, key)?,
                None if !appended => {
                    st.conn.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
                    return Ok(Some(account));
                }
                None => st.enqueue(&account, &op)?,
            };
            st.conn.execute("UPDATE drafts SET purge_op = ?2, open = 0 WHERE id = ?1", params![id, purge])?;
            Ok(Some(account))
        })
    }

    /// Shows again the drafts op `op` was going to discard (its undo), queuing the upload they
    /// skipped meanwhile. A sent draft's cleanup that lost its op (unreadable) is queued again
    /// instead: a sent draft never comes back. Call inside a transaction.
    pub(super) fn undiscard_drafts(&self, op: i64) -> Result<()> {
        let back: Vec<(i64, String, String, bool)> = self
            .conn
            .prepare_cached("SELECT id, account, message_id, sent != 0 FROM drafts WHERE purge_op = ?1 AND purge_op != 0")?
            .query_map(params![op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, account, message_id, sent) in back {
            if sent {
                let purge = self.enqueue(&account, &Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false, left_by: None })?;
                self.conn.execute("UPDATE drafts SET purge_op = ?2 WHERE id = ?1", params![id, purge])?;
                continue;
            }
            self.conn.execute("UPDATE drafts SET purge_op = 0 WHERE id = ?1", params![id])?;
            if self.draft_needs_upload(id)? {
                self.queue_draft_upload(&account, id, 0)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl Store {
    /// Every queued op of the account comes due (a retry's wait passing).
    pub fn ops_due_now(&self, account: &str) {
        self.conn.execute("UPDATE ops SET not_before = 0 WHERE account = ?1", params![account]).unwrap();
    }

    /// Makes saving drafts fail, as a full disk would.
    pub fn break_drafts(&self) {
        self.conn.execute_batch("DROP TABLE drafts").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write from the draft's first lifecycle, stamped `rev`.
    fn at(rev: i64) -> DraftRev {
        DraftRev { lifecycle: 0, rev }
    }

    fn draft(subject: &str) -> Draft {
        Draft { account: "a".into(), to: "bo@x.com".into(), subject: subject.into(), body: "hi".into(), ..Draft::default() }
    }

    /// The ops queued for the account, oldest first.
    fn queued(s: &Store, account: &str) -> Vec<Op> {
        s.ops(account, i64::MAX).unwrap().into_iter().map(|q| q.op).collect()
    }

    fn temp_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("envoy-drafts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.sqlite");
        (dir, db)
    }

    #[test]
    fn saves_apply_in_order_and_only_changes_count() {
        let s = Store::memory();
        assert_eq!(s.save_draft(1, &draft("A"), true, at(10), 100).unwrap(), Some(true), "created");
        assert_eq!(s.save_draft(1, &draft("A"), true, at(11), 101).unwrap(), Some(false), "same content: nothing to upload");
        assert_eq!(s.save_draft(1, &draft("B"), true, at(12), 102).unwrap(), Some(true));
        // A queued save the one made as the window closed overtook arrives late: ignored, but
        // what it holds is saved (the newer save has it too).
        assert_eq!(s.save_draft(1, &draft("stale"), true, at(11), 103).unwrap(), Some(false));
        let d = s.draft(1).unwrap().unwrap();
        assert_eq!((d.draft.subject.as_str(), d.updated, d.visible), ("B", 102, true));
        assert!(d.message_id.starts_with("<1.") && d.message_id.ends_with("@envoy>"), "{}", d.message_id);
    }

    #[test]
    fn the_draft_in_the_composer_comes_back_after_a_restart() {
        let (dir, db) = temp_db("restore");
        {
            let s = Store::open(&db).unwrap();
            s.save_draft(1, &draft("put away"), false, at(1), 100).unwrap();
            s.save_draft(2, &draft("being written"), true, at(2), 200).unwrap();
            // The app dies here: no clean close.
        }
        let s = Store::open(&db).unwrap();
        let back = s.draft_to_restore().unwrap().expect("a draft to restore");
        assert_eq!((back.id, back.draft.subject.as_str()), (2, "being written"));
        assert_eq!(s.drafts(None).unwrap().len(), 2, "the other one is still in Drafts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_while_an_upload_waits_coalesce_into_it() {
        let s = Store::memory();
        s.save_draft(1, &draft("v1"), true, at(1), 100).unwrap();
        assert!(s.queue_draft_upload("a", 1, 105).unwrap());
        for (rev, v) in [(2, "v2"), (3, "v3")] {
            s.save_draft(1, &draft(v), true, at(rev), 100).unwrap();
            assert!(!s.queue_draft_upload("a", 1, 105).unwrap(), "one upload is already waiting");
        }
        assert_eq!(queued(&s, "a"), vec![Op::SaveDraft { draft: 1 }]);
        // Putting the draft away brings it forward.
        assert!(s.queue_draft_upload("a", 1, 100).unwrap());
        assert_eq!(s.ops("a", 100).unwrap().len(), 1);
        // On the wire, it read v3; an edit made now needs an upload of its own.
        let claimed = s.claim_due("a", 100).unwrap();
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        assert_eq!(up.draft.subject, "v3");
        s.save_draft(1, &draft("v4"), true, at(4), 101).unwrap();
        assert!(s.queue_draft_upload("a", 1, 106).unwrap());
        s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap();
        s.op_done(claimed[0].id).unwrap();
        assert!(s.draft_needs_upload(1).unwrap(), "v4 isn't on the server yet");
        assert_eq!(s.draft_upload("a", 1).unwrap().unwrap().draft.subject, "v4");
    }

    #[test]
    fn a_failed_upload_and_one_queued_meanwhile_become_one() {
        let s = Store::memory();
        s.save_draft(1, &draft("v1"), true, at(1), 100).unwrap();
        s.queue_draft_upload("a", 1, 100).unwrap();
        let on_the_wire = s.claim_due("a", 100).unwrap()[0].id;
        // Edited while it's on the wire: an upload of its own, due in a few seconds.
        s.save_draft(1, &draft("v2"), true, at(2), 101).unwrap();
        assert!(s.queue_draft_upload("a", 1, 106).unwrap());
        // The first fails, and would retry in 15 s.
        s.op_failed(on_the_wire, "NO [UNAVAILABLE]", 15, 101).unwrap();
        let waiting = s.ops("a", i64::MAX).unwrap();
        assert_eq!(waiting.len(), 1, "one upload waiting, not two");
        assert_eq!(s.ops("a", 106).unwrap().len(), 1, "due when the edit's was: v2 isn't held back");
        assert_eq!(s.draft_upload("a", 1).unwrap().unwrap().draft.subject, "v2");
    }

    #[test]
    fn a_sent_draft_waits_with_the_message_then_goes() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.draft_upload("a", 1).unwrap(); // it may be on the server now
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello, final"), send, 101)).unwrap();
        let d = s.draft(1).unwrap().unwrap();
        assert!(!d.visible, "hidden from Drafts while it waits in the outbox");
        assert_eq!(d.draft.subject, "hello, final", "with what's being sent");
        assert!(s.draft_upload("a", 1).unwrap().is_none(), "no upload while it's being sent");
        assert_eq!(s.draft_to_restore().unwrap(), None);
        s.op_done(send).unwrap();
        let purge = queued(&s, "a");
        assert!(matches!(&purge[..], [Op::PurgeDraft { draft: Some(1), .. }]), "its server copies go: {purge:?}");
        s.draft_purged(1, 110).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
    }

    #[test]
    fn an_unsent_message_is_kept_unless_its_send_is_queued() {
        let s = Store::memory();
        // The window closed before the engine got to the send: kept, in the composer.
        assert!(s.draft_unsent(1, &draft("hello"), Some(at(1)), 100).unwrap());
        let kept = s.draft(1).unwrap().unwrap();
        assert!(kept.visible && kept.draft.subject == "hello");
        assert_eq!(s.draft_to_restore().unwrap().map(|d| d.id), Some(1));
        // It did get to it: left to go out.
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        assert!(s.draft_unsent(1, &draft("hello"), Some(at(2)), 102).unwrap(), "nothing to keep");
        assert!(!s.draft(1).unwrap().unwrap().visible, "still in the outbox");
        assert_eq!(s.draft_to_restore().unwrap(), None);
    }

    #[test]
    fn an_unsent_copy_overwrites_nothing_newer() {
        let s = Store::memory();
        s.save_draft(1, &draft("newer"), true, at(20), 100).unwrap();
        // The copy of an earlier send that never got queued, written late (from the recovery
        // file, say): newer text is already there.
        assert!(s.draft_unsent(1, &draft("older"), Some(at(10)), 101).unwrap(), "dealt with");
        assert_eq!(s.draft(1).unwrap().unwrap().draft.subject, "newer");
        // One made after it is kept.
        assert!(s.draft_unsent(1, &draft("sent"), Some(at(30)), 102).unwrap());
        assert_eq!(s.draft(1).unwrap().unwrap().draft.subject, "sent");
    }

    #[test]
    fn a_save_cant_touch_a_draft_in_the_outbox() {
        let s = Store::memory();
        s.save_draft(1, &draft("old text"), true, at(1), 100).unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("sent text"), send, 101)).unwrap();
        // A retry of a failed save of older text arrives, stamped later.
        assert_eq!(s.save_draft(1, &draft("old text"), true, at(50), 102).unwrap(), None, "refused");
        assert!(s.cancel_send(send).unwrap());
        let back = s.unsend_draft(send, 103).unwrap().unwrap();
        assert_eq!(back.draft.subject, "sent text", "Undo send gives back what was sent");
        // And after Undo send too: the save was made before the send.
        assert_eq!(s.save_draft(1, &draft("old text"), true, at(51), 104).unwrap(), None);
        assert_eq!(s.draft(1).unwrap().unwrap().draft.subject, "sent text");
        // The composer that got it back saves in its new lifecycle.
        assert_eq!(s.save_draft(1, &draft("edited"), true, DraftRev { rev: 52, ..back.at }, 105).unwrap(), Some(true));
    }

    #[test]
    fn a_sent_draft_stays_gone_whatever_the_clocks_say() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap(); // never uploaded
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 100).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap();
        // The app's copy of what it sent arrives late, stamped by a clock ahead of the
        // database's (or the system clock stepped back since): no clock is consulted.
        assert!(s.draft_unsent(1, &draft("hello"), Some(at(i64::MAX - 1)), 102).unwrap(), "it went out: nothing to keep");
        assert_eq!(s.save_draft(1, &draft("hello"), true, at(i64::MAX), 103).unwrap(), None, "refused");
        assert_eq!(s.draft(1).unwrap(), None, "not brought back to life");
    }

    #[test]
    fn a_draft_that_went_out_stays_gone() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap(); // never uploaded
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 100).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap(); // out, before the app heard (undo send off)
        assert_eq!(s.draft(1).unwrap(), None);
        // The window closes: the app's copy of what it sent, and a save from before, arrive late.
        assert!(s.draft_unsent(1, &draft("hello"), Some(at(2)), 102).unwrap());
        assert_eq!(s.save_draft(1, &draft("hello"), true, at(2), 103).unwrap(), None);
        assert_eq!(s.draft(1).unwrap(), None, "not brought back to life");
    }

    #[test]
    fn a_draft_that_ends_after_newer_ones_keeps_its_fence() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap(); // started long ago, never uploaded
        // 10,000 drafts started after it ended before it.
        s.conn.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1000 UNION ALL SELECT x + 1 FROM n WHERE x < 10999) INSERT INTO draft_ends (id, lifecycle, sent) SELECT x, 0, 0 FROM n").unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 100).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap();
        // The close-time copy of what was sent arrives late.
        assert!(s.draft_unsent(1, &draft("hello"), Some(at(2)), 102).unwrap());
        assert_eq!(s.draft(1).unwrap(), None, "still fenced off");
        let kept: i64 = s.conn.query_row("SELECT COUNT(*) FROM draft_ends", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 10_000, "the latest 10,000 ends kept");
    }

    #[test]
    fn an_earlier_builds_draft_ends_are_carried_over_in_order() {
        let s = Store::memory();
        // As the build before kept them: by draft id, pruned by id.
        s.conn
            .execute_batch(
                "DROP TRIGGER drafts_ended;
                 DROP TABLE draft_ends;
                 CREATE TABLE draft_ends (id INTEGER PRIMARY KEY, lifecycle INTEGER NOT NULL, sent INTEGER NOT NULL);
                 CREATE TRIGGER drafts_end AFTER DELETE ON drafts BEGIN
                     INSERT OR REPLACE INTO draft_ends (id, lifecycle, sent) VALUES (old.id, old.lifecycle, old.sending != 0 OR old.sent != 0);
                     DELETE FROM draft_ends WHERE id < (SELECT id FROM draft_ends ORDER BY id DESC LIMIT 1 OFFSET 9999);
                 END;
                 INSERT INTO draft_ends (id, lifecycle, sent) VALUES (5, 1, 1);",
            )
            .unwrap();
        crate::store::migrate(&s.conn).unwrap();
        assert!(crate::store::has_autoincrement(&s.conn, "draft_ends").unwrap());
        let triggers: Vec<String> =
            s.conn.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'drafts_end%'").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(triggers, vec!["drafts_ended".to_owned()]);
        assert!(s.draft_on_its_way(5).unwrap(), "its ends kept");
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.discard_draft(1, None).unwrap();
        assert_eq!(s.save_draft(1, &draft("hello"), true, at(2), 101).unwrap(), None, "and new ones recorded");
    }

    #[test]
    fn an_earlier_builds_clock_tombstones_give_way() {
        let s = Store::memory();
        s.conn
            .execute_batch(
                "CREATE TABLE draft_tombstones (id INTEGER PRIMARY KEY, gone INTEGER NOT NULL);
                 CREATE TRIGGER drafts_tombstone AFTER DELETE ON drafts BEGIN
                     INSERT OR REPLACE INTO draft_tombstones (id, gone) VALUES (old.id, CAST(unixepoch('subsec') * 1000000 AS INTEGER));
                 END;",
            )
            .unwrap();
        crate::store::migrate(&s.conn).unwrap();
        let left: i64 = s.conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('draft_tombstones', 'drafts_tombstone')", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.discard_draft(1, None).unwrap();
        assert_eq!(s.save_draft(1, &draft("hello"), true, at(2), 101).unwrap(), None, "ended, by id");
    }

    #[test]
    fn a_purged_draft_stays_gone() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.draft_upload("a", 1).unwrap();
        s.discard_draft(1, None).unwrap();
        s.draft_purged(1, 110).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        // A save from before the discard, or after a too-late Undo, whatever its stamp:
        // refused, never taken for saved (the app keeps it as a new draft).
        assert_eq!(s.save_draft(1, &draft("hello"), true, at(2), 111).unwrap(), None);
        assert_eq!(s.save_draft(1, &draft("hello again"), true, at(i64::MAX), 112).unwrap(), None);
        assert_eq!(s.draft(1).unwrap(), None);
        // Nor does an unsent copy: it wasn't sent, so it's for the caller to keep.
        assert!(!s.draft_unsent(1, &draft("hello"), Some(at(3)), 113).unwrap());
        // A new draft gets an id no draft had.
        assert!(s.fresh_draft_id(0).unwrap() > 1);
    }

    #[test]
    fn a_sent_draft_never_comes_back() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.draft_upload("a", 1).unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap();
        let purge = s.ops("a", i64::MAX).unwrap()[0].id;
        // Its cleanup op lost (say, unreadable after a downgrade): queued again, not undone.
        s.op_dropped(purge).unwrap();
        assert!(!s.draft(1).unwrap().unwrap().visible, "still on its way out");
        assert!(matches!(&queued(&s, "a")[..], [Op::PurgeDraft { draft: Some(1), .. }]));
        // A late save (the window closing as it was sent) doesn't bring it back either.
        s.save_draft(1, &draft("hello"), true, at(9), 102).unwrap();
        assert!(!s.draft(1).unwrap().unwrap().visible);
    }

    #[test]
    fn a_sent_draft_that_never_reached_the_server_just_goes() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        assert!(queued(&s, "a").is_empty(), "nothing to purge");
    }

    #[test]
    fn undoing_a_send_brings_the_same_draft_back() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        assert!(s.cancel_send(send).unwrap());
        let back = s.unsend_draft(send, 102).unwrap().expect("back");
        assert_eq!((back.id, back.visible), (1, true));
        assert_eq!(s.drafts(None).unwrap().len(), 1, "not a second draft");
        assert_eq!(s.draft_to_restore().unwrap().map(|d| d.id), Some(1), "in the composer again");
        assert_eq!(queued(&s, "a"), vec![Op::SaveDraft { draft: 1 }], "the upload it skipped is queued");
    }

    #[test]
    fn a_discard_waits_out_its_undo_window() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        assert_eq!(s.discard_draft(1, Some((110, 7))).unwrap(), Some("a".into()));
        assert!(!s.draft(1).unwrap().unwrap().visible);
        assert_eq!(s.discard_draft(1, Some((110, 8))).unwrap(), None, "already on its way out");
        assert!(s.ops("a", 109).unwrap().is_empty(), "held for the window");
        // Undo: back, as it was, with the upload it skipped queued.
        assert_eq!(s.undo_batch(7).unwrap().released, 1);
        assert!(s.draft(1).unwrap().unwrap().visible);
        assert_eq!(queued(&s, "a"), vec![Op::SaveDraft { draft: 1 }]);
        // Discarded for good: no upload, and the purge deletes it.
        s.discard_draft(1, Some((110, 9))).unwrap();
        assert!(s.draft_upload("a", 1).unwrap().is_none());
        assert!(matches!(&queued(&s, "a")[..], [Op::SaveDraft { .. }, Op::PurgeDraft { draft: Some(1), .. }]));
        s.draft_purged(1, 111).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
    }

    #[test]
    fn without_undo_a_draft_the_server_never_had_goes_at_once() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        s.discard_draft(1, None).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        assert!(queued(&s, "a").is_empty());
    }

    #[test]
    fn a_draft_saved_after_a_too_late_undo_survives_its_purge() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        assert!(s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap());
        s.discard_draft(1, Some((110, 7))).unwrap();
        s.claim_due("a", 110).unwrap(); // the purge is on the wire: undo is too late
        assert_eq!(s.undo_batch(7).unwrap().in_flight, 1);
        // The app put the composer back anyway, and it saves.
        s.save_draft(1, &draft("hello again"), true, at(2), 111).unwrap();
        s.draft_purged(1, 112).unwrap();
        let d = s.draft(1).unwrap().expect("kept");
        assert_eq!(d.draft.subject, "hello again");
        assert!(s.draft_needs_upload(1).unwrap(), "and uploaded again, its copies being gone");
    }

    #[test]
    fn listing_drafts_reads_only_the_drafts_mailbox() {
        let s = Store::memory();
        s.upsert_mailbox("a", "INBOX", "Inbox", crate::model::Role::Inbox, "/", true).unwrap();
        s.upsert_mailbox("a", "Drafts", "Drafts", crate::model::Role::Drafts, "/", true).unwrap();
        s.transaction(|st| {
            for uid in 1..=5_000u32 {
                st.conn.execute(
                    "INSERT INTO messages (account, mailbox, uid, message_id, thread, date) VALUES ('a', ?1, ?2, ?3, ?3, ?2)",
                    params![if uid % 500 == 0 { "Drafts" } else { "INBOX" }, uid, format!("<{uid}@x>")],
                )?;
            }
            Ok(())
        })
        .unwrap();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        for stats in ["without statistics", "after ANALYZE"] {
            if stats == "after ANALYZE" {
                s.conn.execute_batch("ANALYZE").unwrap();
            }
            let plan: Vec<String> =
                s.conn.prepare(&format!("EXPLAIN QUERY PLAN {DRAFT_COPIES}")).unwrap().query_map(params!["a"], |r| r.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            let on_messages: Vec<&String> = plan.iter().filter(|d| d.contains(" m ")).collect();
            assert!(
                !on_messages.is_empty() && on_messages.iter().all(|d| d.starts_with("SEARCH m USING") && d.contains("(account=? AND mailbox=?)")),
                "{stats}: {plan:?}"
            );
        }
        assert!(s.draft_copies(Some("a")).unwrap().is_empty());
        // And it finds a copy when there is one.
        let mid = s.draft(1).unwrap().unwrap().message_id;
        s.conn.execute("UPDATE messages SET message_id = ?1 WHERE uid = 500", params![mid]).unwrap();
        let copy: i64 = s.conn.query_row("SELECT id FROM messages WHERE uid = 500", [], |r| r.get(0)).unwrap();
        assert_eq!(s.draft_copies(None).unwrap(), vec![(copy, 1)]);
        assert_eq!(s.draft_copies(Some("b")).unwrap(), vec![]);
    }

    #[test]
    fn an_upload_that_finishes_after_a_move_is_fenced_off() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        // While it's on the wire, the draft moves to b.
        let moved = Draft { account: "b".into(), ..draft("hello") };
        s.save_draft(1, &moved, true, at(2), 101).unwrap();
        let mid = s.draft(1).unwrap().unwrap().message_id;
        assert!(!s.draft_uploaded(1, &up, "Drafts", Some((7, 21)), true).unwrap(), "not recorded");
        assert_eq!(s.draft_copy(1).unwrap(), None, "the draft has no copy on b");
        assert!(s.draft_needs_upload(1).unwrap(), "and b still needs one");
        let on_a = queued(&s, "a");
        assert!(on_a.contains(&Op::PurgeDraft { message_id: mid, draft: None, copy: Some(("Drafts".into(), 7, 21)), exact: true, left_by: Some((1, 0)) }), "a's copy goes, on a, by UID: {on_a:?}");
        assert!(s.draft_upload("b", 1).unwrap().unwrap().previous.is_none(), "b's upload replaces nothing");
    }

    fn file(id: i64, name: &str, size: usize) -> DraftAttachment {
        DraftAttachment { id, filename: name.into(), mime: crate::attach::type_for(name).into(), size, status: FileStatus::Attached }
    }

    fn from_disk(path: &str) -> FileSource {
        FileSource::Path(path.into())
    }

    /// Attaches `name` to draft `id` as file `file`, its bytes `data`, as the engine does.
    fn attach(s: &Store, id: i64, file: i64, name: &str, data: &[u8]) {
        let listed = DraftAttachment { status: FileStatus::Attaching, ..self::file(file, name, 0) };
        assert_eq!(s.attaching(id, &[(listed, from_disk(name))]).unwrap(), Some(vec![true]));
        assert_eq!(s.file_in(file, data, "", 100).unwrap().len(), 1);
    }

    fn stored(s: &Store, table: &str) -> i64 {
        s.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_file_being_attached_is_kept_with_its_draft_until_its_bytes_are_in() {
        let (dir, db) = temp_db("files");
        let pdf = file(7, "plan.pdf", 4);
        {
            let s = Store::open(&db).unwrap();
            let listed = DraftAttachment { status: FileStatus::Attaching, ..file(7, "plan.pdf", 0) };
            assert_eq!(s.attaching(1, &[(listed.clone(), from_disk("/x/plan.pdf"))]).unwrap(), None, "no draft, nothing kept: a file never outlives its draft");
            s.save_draft(1, &draft("plan"), true, at(1), 100).unwrap();
            assert_eq!(s.attaching(1, &[(listed.clone(), from_disk("/x/plan.pdf"))]).unwrap(), Some(vec![true]));
            assert_eq!(s.draft(1).unwrap().unwrap().draft.attachments, vec![listed], "listed at once, as being attached");
            // The app dies here, the file half read.
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.attaching_files().unwrap(), vec![(1, 7, from_disk("/x/plan.pdf"))], "a restart finds it, and where it comes from");
        assert_eq!(s.file_in(7, b"%PDF", "", 200).unwrap(), vec![(1, pdf.clone())]);
        let back = s.draft_to_restore().unwrap().unwrap();
        assert_eq!(back.draft.attachments, vec![pdf.clone()], "attached");
        assert_eq!(s.attached_files(1).unwrap(), vec![(pdf, b"%PDF".to_vec())], "with its bytes");
        assert!(s.draft_needs_upload(1).unwrap() && s.attaching_files().unwrap().is_empty());
        // Typing on rewrites the text, never the file; saves don't list files at all.
        s.save_draft(1, &draft("plan, v2"), true, at(3), 202).unwrap();
        assert_eq!(s.draft(1).unwrap().unwrap().draft.attachments.len(), 1);
        s.discard_draft(1, None).unwrap();
        assert_eq!((stored(&s, "draft_files"), stored(&s, "draft_attachments")), (0, 0), "gone with the draft");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_never_adds_or_takes_off_a_file() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        attach(&s, 1, 10, "a.txt", b"a");
        // A save listing another file, or none, made before or after: the store's files stand.
        s.save_draft(1, &Draft { attachments: vec![file(11, "b.txt", 1)], ..draft("x") }, true, at(2), 101).unwrap();
        s.save_draft(1, &draft("x, later"), true, at(3), 102).unwrap();
        assert_eq!(s.draft(1).unwrap().unwrap().draft.attachments, vec![file(10, "a.txt", 1)]);
        assert_eq!(s.draft_upload("a", 1).unwrap().unwrap().draft.attachments, vec![file(10, "a.txt", 1)], "uploaded with it");
    }

    #[test]
    fn a_file_id_is_never_taken_twice_and_bytes_never_replaced() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        attach(&s, 1, 5, "a.pdf", b"AAAA");
        let again = DraftAttachment { status: FileStatus::Attaching, ..file(5, "b.png", 0) };
        assert_eq!(s.attaching(1, &[(again, from_disk("b.png"))]).unwrap(), Some(vec![false]), "refused");
        assert!(s.file_in(5, b"BBBB", "", 101).unwrap().is_empty(), "a late answer for id 5 changes nothing");
        assert_eq!(s.attached_files(1).unwrap(), vec![(file(5, "a.pdf", 4), b"AAAA".to_vec())]);
        // Taken off, its id still isn't given again, and an answer still on its way for it is
        // dropped.
        let pending = DraftAttachment { status: FileStatus::Attaching, ..file(6, "slow.mov", 0) };
        s.attaching(1, &[(pending.clone(), from_disk("slow.mov"))]).unwrap();
        assert!(!s.remove_file(1, 6, 102).unwrap(), "it wasn't attached yet");
        assert!(s.file_in(6, b"late", "", 103).unwrap().is_empty());
        assert_eq!(s.attaching(1, &[(pending, from_disk("other.mov"))]).unwrap(), Some(vec![false]));
        assert_eq!(s.files_of(1).unwrap(), vec![file(5, "a.pdf", 4)]);
    }

    #[test]
    fn a_file_taken_off_goes_with_its_bytes_and_the_draft_gets_an_upload() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        attach(&s, 1, 10, "a.txt", b"a");
        attach(&s, 1, 11, "b.txt", b"b");
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap();
        assert!(s.remove_file(1, 11, 101).unwrap());
        assert_eq!(s.files_of(1).unwrap(), vec![file(10, "a.txt", 1)]);
        assert_eq!(stored(&s, "draft_files"), 1, "its bytes gone");
        assert!(s.draft_needs_upload(1).unwrap(), "the server's copy still has it");
        assert!(!s.remove_file(1, 11, 102).unwrap(), "once");
    }

    #[test]
    fn a_file_that_cant_be_read_says_why_and_stays_listed() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        let listed = DraftAttachment { status: FileStatus::Attaching, ..file(3, "gone.pdf", 0) };
        s.attaching(1, &[(listed, from_disk("/gone.pdf"))]).unwrap();
        assert_eq!(s.file_failed(3, "Couldn't read gone.pdf: not found").unwrap(), vec![1]);
        assert_eq!(s.files_of(1).unwrap()[0].status, FileStatus::Failed("Couldn't read gone.pdf: not found".into()));
        assert!(s.attaching_files().unwrap().is_empty() && s.attached_files(1).unwrap().is_empty());
    }

    #[test]
    fn a_draft_carrying_on_anothers_text_takes_its_files_even_those_on_their_way() {
        let s = Store::memory();
        s.save_draft(1, &draft("plan"), true, at(1), 100).unwrap();
        attach(&s, 1, 10, "plan.pdf", b"%PDF");
        let slow = DraftAttachment { status: FileStatus::Attaching, ..file(11, "slow.mov", 0) };
        s.attaching(1, &[(slow, from_disk("slow.mov"))]).unwrap();
        // A save refused under id 1 carries on as draft 2.
        assert_eq!(s.save_draft_from(2, &draft("plan"), true, at(1), 102, Some(1)).unwrap(), Some(true));
        assert_eq!(s.attached_files(2).unwrap()[0].1, b"%PDF".to_vec());
        // The slow one's bytes go to both.
        let done: Vec<i64> = s.file_in(11, b"MOV", "", 103).unwrap().into_iter().map(|(d, _)| d).collect();
        assert_eq!(done, vec![1, 2]);
        assert_eq!(s.attached_files(2).unwrap().len(), 2);
        // One refused itself leaves nothing behind.
        s.save_draft(3, &draft("gone"), true, at(1), 100).unwrap();
        s.discard_draft(3, None).unwrap();
        let before = (stored(&s, "draft_files"), stored(&s, "draft_attachments"));
        assert_eq!(s.save_draft_from(3, &draft("gone"), true, at(2), 104, Some(1)).unwrap(), None);
        assert_eq!((stored(&s, "draft_files"), stored(&s, "draft_attachments")), before, "no file without its draft");
    }

    #[test]
    fn a_file_taken_off_before_it_was_listed_stays_off() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        // Taken off on close before the engine listed it; its attach, still queued, comes after.
        assert!(!s.remove_file(1, 10, 101).unwrap());
        let listed = DraftAttachment { status: FileStatus::Attaching, ..file(10, "a.txt", 0) };
        assert_eq!(s.attaching(1, &[(listed, from_disk("a.txt"))]).unwrap(), Some(vec![false]));
        assert!(s.files_of(1).unwrap().is_empty());
        // Nothing kept for a draft that isn't there.
        assert!(!s.remove_file(2, 11, 102).unwrap());
        assert_eq!(stored(&s, "draft_attachments"), 1);
    }

    #[test]
    fn a_late_carry_over_brings_back_no_file_taken_off() {
        let s = Store::memory();
        s.save_draft(1, &draft("x"), true, at(1), 100).unwrap();
        attach(&s, 1, 10, "a.txt", b"a");
        s.save_draft_from(2, &draft("x"), true, at(1), 101, Some(1)).unwrap();
        assert!(s.remove_file(2, 10, 102).unwrap());
        // A save still carrying where its files came from (a retry, say) copies again.
        s.save_draft_from(2, &draft("x, again"), true, at(2), 103, Some(1)).unwrap();
        assert!(s.files_of(2).unwrap().is_empty());
        let bytes: i64 = s.conn.query_row("SELECT COUNT(*) FROM draft_files WHERE draft = 2", [], |r| r.get(0)).unwrap();
        assert_eq!(bytes, 0, "no bytes behind the file's tombstone");
    }

    #[test]
    fn files_of_drafts_from_an_earlier_build_are_listed() {
        let s = Store::memory();
        s.save_draft(1, &draft("old"), true, at(1), 100).unwrap();
        // As the build before kept them: listed in the content, bytes in draft_files.
        let old = Draft { attachments: vec![DraftAttachment { id: 0, filename: "plan.pdf".into(), mime: "application/pdf".into(), size: 4, ..Default::default() }], ..draft("old") };
        s.conn.execute("UPDATE drafts SET content = ?2 WHERE id = ?1", params![1, json(&old)]).unwrap();
        s.conn.execute("INSERT INTO draft_files (draft, id, data) VALUES (1, 0, x'25504446')", []).unwrap();
        crate::store::migrate(&s.conn).unwrap();
        assert_eq!(s.attached_files(1).unwrap(), vec![(file(0, "plan.pdf", 4), b"%PDF".to_vec())]);
    }

    #[test]
    fn moving_a_draft_to_another_account_purges_the_old_copies() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, at(1), 100).unwrap();
        let mid = s.draft(1).unwrap().unwrap().message_id;
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        assert!(s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap());
        let moved = Draft { account: "b".into(), ..draft("hello") };
        assert_eq!(s.save_draft(1, &moved, true, at(2), 101).unwrap(), Some(true));
        assert_eq!(queued(&s, "a"), vec![Op::PurgeDraft { message_id: mid, draft: None, copy: Some(("Drafts".into(), 1, 1)), exact: true, left_by: Some((1, 0)) }], "the copy on record, by UID");
        assert!(s.draft_upload("a", 1).unwrap().is_none(), "not uploaded to the old account");
        assert!(s.draft_upload("b", 1).unwrap().is_some(), "but to the new one");
    }
}
