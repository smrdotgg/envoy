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
//! Once a draft is gone for good (sent, purged, discarded), a tombstone remembers when, for 90
//! days: a write that was on its way from before that (a queued save, the app's copy of a
//! message it sent, written as the window closed) can't bring it back. Ids are never reused,
//! so a save stamped after the tombstone is a real edit (a too-late Undo put the draft back in
//! the composer) and starts it again.
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
use crate::model::{Draft, DraftAttachment, MessageId};

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

const COLS: &str = "d.id, d.account, d.content, d.message_id, d.updated, d.sending = 0 AND d.purge_op = 0";

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
    Ok(LocalDraft { id: r.get(0)?, account, draft, message_id: r.get(3)?, updated: r.get(4)?, visible: r.get(5)? })
}

/// A Message-ID for a new draft. The angle brackets make a search for it (a substring match in
/// IMAP) find this draft's copies and nothing else.
fn new_message_id(id: i64) -> String {
    format!("<{id}.{:08x}.draft@envoy>", rand::random::<u32>())
}

impl Store {
    /// Saves the composer's content as draft `id`, creating it on the first save. `rev` orders
    /// saves (the app stamps each one): a save no newer than the last one applied, such as a
    /// queued save overtaken by the one made as the window closed, changes nothing. `open`: it's
    /// in the composer, to be opened again after a restart.
    ///
    /// Saving shows a draft whose discard is on its way to the server (a too-late undo put it
    /// back in the composer; [`Store::draft_purged`] keeps it). Moving a draft to another
    /// account queues the deletion of its copies on the old account's server. Returns whether
    /// the content changed.
    pub fn save_draft(&self, id: i64, draft: &Draft, open: bool, rev: i64, now: i64) -> Result<bool> {
        self.transaction(|st| Ok(st.put_draft(id, draft, open, Some(rev), rev, now)?.unwrap_or(false)))
    }

    /// [`Store::save_draft`]; `rev: None` writes whatever the stored rev. `stamp`: when the
    /// write was made, against the draft's tombstone. None when it was too old.
    /// Call inside a transaction.
    fn put_draft(&self, id: i64, draft: &Draft, open: bool, rev: Option<i64>, stamp: i64, now: i64) -> Result<Option<bool>> {
        // Gone for good since before this write was made: stays gone.
        let gone: Option<i64> = self.conn.query_row("SELECT gone FROM draft_tombstones WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        match gone {
            Some(at) if stamp <= at => return Ok(None),
            Some(_) => {
                self.conn.execute("DELETE FROM draft_tombstones WHERE id = ?1", params![id])?;
            }
            None => {}
        }
        let content = json(draft);
        type Row = (String, String, String, i64, bool, Option<(String, u32, u32)>, bool);
        let old: Option<Row> = self
            .conn
            .query_row(
                "SELECT account, content, message_id, rev, appended != 0, server_mailbox, server_validity, server_uid, server_account = account, unconfirmed != 0 FROM drafts WHERE id = ?1",
                params![id],
                |r| {
                    let copy = (r.get::<_, u32>(7)? != 0 && r.get::<_, bool>(8)?).then(|| Ok::<_, rusqlite::Error>((r.get(5)?, r.get(6)?, r.get(7)?))).transpose()?;
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, copy, r.get(9)?))
                },
            )
            .optional()?;
        let Some((account, old_content, message_id, old_rev, appended, copy, unconfirmed)) = old else {
            self.conn.execute(
                "INSERT INTO drafts (id, account, content, message_id, rev, updated, version, open) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                params![id, draft.account, content, new_message_id(id), rev.unwrap_or(0), now, open],
            )?;
            return Ok(Some(true));
        };
        if rev.is_some_and(|r| r <= old_rev) {
            return Ok(None);
        }
        let moved = account != draft.account;
        if moved && appended {
            // Its copies on the old account's server go, there: the one on record by UID, and
            // by Message-ID only if an upload may have left one no one knows the UID of.
            let exact = copy.is_some() && !unconfirmed;
            self.enqueue(&account, &Op::PurgeDraft { message_id, draft: None, copy, exact })?;
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

    /// Any draft, shown or not.
    pub fn draft(&self, id: i64) -> Result<Option<LocalDraft>> {
        Ok(self.conn.query_row(&format!("SELECT {COLS} FROM drafts d WHERE d.id = ?1"), params![id], row).optional()?)
    }

    /// Every draft, of one account or all, newest first, shown or not: what the Drafts list is
    /// merged with.
    pub fn drafts(&self, account: Option<&str>) -> Result<Vec<LocalDraft>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLS} FROM drafts d WHERE ?1 IS NULL OR d.account = ?1 ORDER BY d.updated DESC, d.id DESC"))?;
        let rows = stmt.query_map(params![account], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The draft that was in the composer when the app last closed, to open again.
    pub fn draft_to_restore(&self) -> Result<Option<LocalDraft>> {
        let sql = format!("SELECT {COLS} FROM drafts d WHERE d.open = 1 AND d.sending = 0 AND d.purge_op = 0 ORDER BY d.updated DESC, d.id DESC LIMIT 1");
        Ok(self.conn.query_row(&sql, [], row).optional()?)
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
        Ok(self.conn.query_row(&sql, params![id], row).optional()?)
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
            let id: i64 = st.conn.query_row("SELECT MAX(COALESCE(MAX(id), 0) + 1, ?1) FROM drafts", params![now.saturating_mul(1_000_000)], |r| r.get(0))?;
            let draft = Draft { account: account.clone(), ..draft.clone() };
            let fresh = message_id.is_empty();
            let message_id = if fresh { new_message_id(id) } else { message_id };
            st.conn.execute(
                "INSERT INTO drafts (id, account, content, message_id, updated, version, server_version, appended, open) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6, 1)",
                params![id, account, json(&draft), message_id, now, if fresh { 0 } else { 1 }],
            )?;
            for (a, data) in draft.attachments.iter().zip(files) {
                st.conn.execute("INSERT OR REPLACE INTO draft_files (draft, id, data) VALUES (?1, ?2, ?3)", params![id, a.id, data])?;
            }
            if fresh {
                let op = st.enqueue(&account, &Op::Expunge { mailbox, uid })?;
                st.hide_local(message, op)?;
                st.queue_draft_upload(&account, id, now)?;
            }
            st.draft(id)
        })
    }

    /// The bytes of a draft's attachments, with them. An error if any is missing: a draft is
    /// never sent or saved without a file it lists.
    pub fn draft_files(&self, id: i64, attachments: &[DraftAttachment]) -> Result<Vec<(DraftAttachment, Vec<u8>)>> {
        let mut stmt = self.conn.prepare_cached("SELECT data FROM draft_files WHERE draft = ?1 AND id = ?2")?;
        attachments
            .iter()
            .map(|a| {
                let data: Option<Vec<u8>> = stmt.query_row(params![id, a.id], |r| r.get(0)).optional()?;
                data.map(|d| (a.clone(), d)).ok_or_else(|| anyhow::anyhow!("The attached file {} is missing from this draft", a.filename))
            })
            .collect()
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
                st.enqueue(&up.account, &Op::PurgeDraft { message_id: up.message_id.clone(), draft: None, copy, exact })?;
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
        // Being sent now: whatever came before.
        self.put_draft(id, draft, false, None, i64::MAX, now)?;
        self.conn.execute("UPDATE drafts SET sending = ?2 WHERE id = ?1", params![id, op])?;
        Ok(())
    }

    /// Keeps a message that wasn't sent as a draft, in the composer, with what was being sent:
    /// its send couldn't be queued, or the window closed before the engine confirmed it.
    /// `stamp`: when it was sent. A draft whose send is queued, or went out (gone since), is
    /// left alone.
    pub fn draft_unsent(&self, id: i64, draft: &Draft, stamp: i64, now: i64) -> Result<Option<LocalDraft>> {
        self.transaction(|st| {
            let on_its_way: bool = st.conn.query_row("SELECT sending != 0 OR sent != 0 FROM drafts WHERE id = ?1", params![id], |r| r.get(0)).optional()?.unwrap_or(false);
            if !on_its_way {
                st.put_draft(id, draft, true, None, stamp, now)?;
            }
            st.draft(id)
        })
    }

    /// Brings back the draft whose send (op `op`) was undone, in the composer, queuing the
    /// upload it skipped while it waited. Call inside a transaction, with the cancel.
    pub fn unsend_draft(&self, op: i64, now: i64) -> Result<Option<LocalDraft>> {
        let id: Option<i64> = self.conn.query_row("SELECT id FROM drafts WHERE sending = ?1 AND sending != 0", params![op], |r| r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        self.conn.execute("UPDATE drafts SET sending = 0, open = 1 WHERE id = ?1", params![id])?;
        let back = self.draft(id)?;
        if let Some(d) = &back
            && self.draft_needs_upload(id)?
        {
            self.queue_draft_upload(&d.account, id, now)?;
        }
        Ok(back)
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
                let purge = self.enqueue(&account, &Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false })?;
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
            let op = Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false };
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
                let purge = self.enqueue(&account, &Op::PurgeDraft { message_id, draft: Some(id), copy: None, exact: false })?;
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
        assert!(s.save_draft(1, &draft("A"), true, 10, 100).unwrap(), "created");
        assert!(!s.save_draft(1, &draft("A"), true, 11, 101).unwrap(), "same content: nothing to upload");
        assert!(s.save_draft(1, &draft("B"), true, 12, 102).unwrap());
        // A queued save the one made as the window closed overtook arrives late: ignored.
        assert!(!s.save_draft(1, &draft("stale"), true, 11, 103).unwrap());
        let d = s.draft(1).unwrap().unwrap();
        assert_eq!((d.draft.subject.as_str(), d.updated, d.visible), ("B", 102, true));
        assert!(d.message_id.starts_with("<1.") && d.message_id.ends_with("@envoy>"), "{}", d.message_id);
    }

    #[test]
    fn the_draft_in_the_composer_comes_back_after_a_restart() {
        let (dir, db) = temp_db("restore");
        {
            let s = Store::open(&db).unwrap();
            s.save_draft(1, &draft("put away"), false, 1, 100).unwrap();
            s.save_draft(2, &draft("being written"), true, 2, 200).unwrap();
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
        s.save_draft(1, &draft("v1"), true, 1, 100).unwrap();
        assert!(s.queue_draft_upload("a", 1, 105).unwrap());
        for (rev, v) in [(2, "v2"), (3, "v3")] {
            s.save_draft(1, &draft(v), true, rev, 100).unwrap();
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
        s.save_draft(1, &draft("v4"), true, 4, 101).unwrap();
        assert!(s.queue_draft_upload("a", 1, 106).unwrap());
        s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap();
        s.op_done(claimed[0].id).unwrap();
        assert!(s.draft_needs_upload(1).unwrap(), "v4 isn't on the server yet");
        assert_eq!(s.draft_upload("a", 1).unwrap().unwrap().draft.subject, "v4");
    }

    #[test]
    fn a_sent_draft_waits_with_the_message_then_goes() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
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
        let kept = s.draft_unsent(1, &draft("hello"), 1, 100).unwrap().unwrap();
        assert!(kept.visible && kept.draft.subject == "hello");
        assert_eq!(s.draft_to_restore().unwrap().map(|d| d.id), Some(1));
        // It did get to it: left to go out.
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.draft_unsent(1, &draft("hello"), 2, 102).unwrap();
        assert!(!s.draft(1).unwrap().unwrap().visible, "still in the outbox");
        assert_eq!(s.draft_to_restore().unwrap(), None);
    }

    #[test]
    fn a_draft_that_went_out_stays_gone() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap(); // never uploaded
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 100).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap(); // out, before the app heard (undo send off)
        assert_eq!(s.draft(1).unwrap(), None);
        // The window closes: the app's copy of what it sent, and a save from before, arrive late.
        s.draft_unsent(1, &draft("hello"), 2, 102).unwrap();
        s.save_draft(1, &draft("hello"), true, 2, 103).unwrap();
        assert_eq!(s.draft(1).unwrap(), None, "not brought back to life");
    }

    #[test]
    fn a_purged_draft_stays_gone_until_edited_again() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        s.draft_upload("a", 1).unwrap();
        s.discard_draft(1, None).unwrap();
        s.draft_purged(1, 110).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        s.save_draft(1, &draft("hello"), true, 2, 111).unwrap();
        assert_eq!(s.draft(1).unwrap(), None, "a save from before the discard changes nothing");
        // A too-late Undo put it back in the composer, and it's edited: a save made now counts.
        let now = chrono::Utc::now().timestamp_micros() + 1_000_000;
        s.save_draft(1, &draft("hello again"), true, now, 112).unwrap();
        assert_eq!(s.draft(1).unwrap().map(|d| d.draft.subject), Some("hello again".into()));
    }

    #[test]
    fn a_sent_draft_never_comes_back() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
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
        s.save_draft(1, &draft("hello"), true, 9, 102).unwrap();
        assert!(!s.draft(1).unwrap().unwrap().visible);
    }

    #[test]
    fn a_sent_draft_that_never_reached_the_server_just_goes() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        let send = s.enqueue_at("a", &Op::Send { raw_b64: String::new(), from: String::new(), to: vec![], sent_mailbox: None, reply_of: None }, 105).unwrap();
        s.transaction(|st| st.draft_sending(1, &draft("hello"), send, 101)).unwrap();
        s.op_done(send).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        assert!(queued(&s, "a").is_empty(), "nothing to purge");
    }

    #[test]
    fn undoing_a_send_brings_the_same_draft_back() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
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
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
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
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        s.discard_draft(1, None).unwrap();
        assert_eq!(s.draft(1).unwrap(), None);
        assert!(queued(&s, "a").is_empty());
    }

    #[test]
    fn a_draft_saved_after_a_too_late_undo_survives_its_purge() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        assert!(s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap());
        s.discard_draft(1, Some((110, 7))).unwrap();
        s.claim_due("a", 110).unwrap(); // the purge is on the wire: undo is too late
        assert_eq!(s.undo_batch(7).unwrap().in_flight, 1);
        // The app put the composer back anyway, and it saves.
        s.save_draft(1, &draft("hello again"), true, 2, 111).unwrap();
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
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
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
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        // While it's on the wire, the draft moves to b.
        let moved = Draft { account: "b".into(), ..draft("hello") };
        s.save_draft(1, &moved, true, 2, 101).unwrap();
        let mid = s.draft(1).unwrap().unwrap().message_id;
        assert!(!s.draft_uploaded(1, &up, "Drafts", Some((7, 21)), true).unwrap(), "not recorded");
        assert_eq!(s.draft_copy(1).unwrap(), None, "the draft has no copy on b");
        assert!(s.draft_needs_upload(1).unwrap(), "and b still needs one");
        let on_a = queued(&s, "a");
        assert!(on_a.contains(&Op::PurgeDraft { message_id: mid, draft: None, copy: Some(("Drafts".into(), 7, 21)), exact: true }), "a's copy goes, on a, by UID: {on_a:?}");
        assert!(s.draft_upload("b", 1).unwrap().unwrap().previous.is_none(), "b's upload replaces nothing");
    }

    #[test]
    fn moving_a_draft_to_another_account_purges_the_old_copies() {
        let s = Store::memory();
        s.save_draft(1, &draft("hello"), true, 1, 100).unwrap();
        let mid = s.draft(1).unwrap().unwrap().message_id;
        let up = s.draft_upload("a", 1).unwrap().unwrap();
        assert!(s.draft_uploaded(1, &up, "Drafts", Some((1, 1)), true).unwrap());
        let moved = Draft { account: "b".into(), ..draft("hello") };
        assert!(s.save_draft(1, &moved, true, 2, 101).unwrap());
        assert_eq!(queued(&s, "a"), vec![Op::PurgeDraft { message_id: mid, draft: None, copy: Some(("Drafts".into(), 1, 1)), exact: true }], "the copy on record, by UID");
        assert!(s.draft_upload("a", 1).unwrap().is_none(), "not uploaded to the old account");
        assert!(s.draft_upload("b", 1).unwrap().is_some(), "but to the new one");
    }
}
