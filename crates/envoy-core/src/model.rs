//! Types shared by the engine and the UI.

use serde::{Deserialize, Serialize};

pub type MessageId = i64;

pub mod flags {
    pub const SEEN: u32 = 1;
    pub const FLAGGED: u32 = 2;
    pub const ANSWERED: u32 = 4;
    pub const DRAFT: u32 = 8;
    pub const DELETED: u32 = 16;
}

/// What a mailbox is for, from IMAP SPECIAL-USE attributes (or well-known names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Role {
    Inbox,
    Flagged,
    Drafts,
    Sent,
    Archive,
    All,
    Junk,
    Trash,
    Important,
    Other,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Inbox => "inbox",
            Role::Flagged => "flagged",
            Role::Drafts => "drafts",
            Role::Sent => "sent",
            Role::Archive => "archive",
            Role::All => "all",
            Role::Junk => "junk",
            Role::Trash => "trash",
            Role::Important => "important",
            Role::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Role {
        match s {
            "inbox" => Role::Inbox,
            "flagged" => Role::Flagged,
            "drafts" => Role::Drafts,
            "sent" => Role::Sent,
            "archive" => Role::Archive,
            "all" => Role::All,
            "junk" => Role::Junk,
            "trash" => Role::Trash,
            "important" => Role::Important,
            _ => Role::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Inbox => "Inbox",
            Role::Flagged => "Starred",
            Role::Drafts => "Drafts",
            Role::Sent => "Sent",
            Role::Archive => "Archive",
            Role::All => "All Mail",
            Role::Junk => "Spam",
            Role::Trash => "Trash",
            Role::Important => "Important",
            Role::Other => "",
        }
    }

    /// Sidebar order.
    pub fn rank(self) -> u8 {
        match self {
            Role::Inbox => 0,
            Role::Flagged => 1,
            Role::Important => 2,
            Role::Sent => 3,
            Role::Drafts => 4,
            Role::Archive => 5,
            Role::All => 6,
            Role::Junk => 7,
            Role::Trash => 8,
            Role::Other => 9,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mailbox {
    pub account: String,
    /// The IMAP name (`[Gmail]/Sent Mail`).
    pub name: String,
    /// What the sidebar shows (`Sent`).
    pub display: String,
    pub role: Role,
    pub unread: u32,
    pub total: u32,
    /// Nesting depth by the server's delimiter, for labels like `Work/Clients`.
    pub depth: u8,
    /// Mailboxes that can't be selected (`\Noselect` parents).
    pub selectable: bool,
}

/// Which messages a list shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// One mailbox of one account.
    Mailbox { account: String, mailbox: String },
    /// A role across every account (the unified inbox) or one account when `account` is set.
    Role { account: Option<String>, role: Role },
    /// Starred (flagged) messages in every synced mailbox.
    Starred { account: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub email: String,
}

impl Address {
    pub fn display(&self) -> &str {
        if self.name.trim().is_empty() { &self.email } else { &self.name }
    }

    pub fn formatted(&self) -> String {
        if self.name.trim().is_empty() { self.email.clone() } else { format!("{} <{}>", self.name, self.email) }
    }
}

/// One row of the message list.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub id: MessageId,
    pub account: String,
    pub mailbox: String,
    pub uid: u32,
    pub thread: String,
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    /// Unix seconds.
    pub date: i64,
    pub flags: u32,
    pub preview: String,
    pub has_attachments: bool,
    /// In conversation view: how many messages the conversation has in this view.
    pub thread_count: u32,
    /// In conversation view: the participants' first names (`Ana, me, Bo`).
    pub participants: String,
    /// Some message in the conversation is unread.
    pub thread_unread: bool,
}

impl Summary {
    pub fn seen(&self) -> bool {
        self.flags & flags::SEEN != 0
    }
    pub fn flagged(&self) -> bool {
        self.flags & flags::FLAGGED != 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub index: usize,
    pub filename: String,
    pub mime: String,
    pub size: usize,
}

/// An opened message's content.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    pub id: MessageId,
    pub message_id: String,
    pub references: String,
    pub cc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub text: Option<String>,
    pub html: Option<String>,
    /// The HTML turned into Markdown (for quoting in replies).
    pub markdown: Option<String>,
    /// The HTML as a layout tree, for drawing natively.
    pub doc: Option<std::sync::Arc<crate::html::Doc>>,
    pub attachments: Vec<Attachment>,
    /// Pictures shown inside the message (`cid:`), which aren't listed as files: a forward,
    /// written as plain text, takes them along as files.
    pub inline: Vec<Attachment>,
}

/// One message of an opened conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMessage {
    pub summary: Summary,
    pub body: Option<Body>,
}

/// What the composer holds: a message being written, or one being sent. Saved as JSON in the
/// local drafts table, so fields only ever get added, with defaults.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub account: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: String,
    pub references: String,
    /// The message this replies to, to mark it answered after sending.
    pub reply_of: Option<MessageId>,
    /// Its files, as the store has them: the store owns this list (a file is added and taken
    /// off by commands of its own, and a save's list is ignored), and each file's bytes are in
    /// it with the draft, written once, so saving the text as it's typed never rewrites them.
    pub attachments: Vec<DraftAttachment>,
}

/// A file of a draft: what the composer shows. Its id is unique and never given twice (the
/// app's stamps); the bytes are in the store under the draft's id and this one.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftAttachment {
    pub id: i64,
    pub filename: String,
    /// `type/subtype`, with the `charset` of text that names one.
    pub mime: String,
    pub size: usize,
    pub status: FileStatus,
}

/// Where a file of a draft stands.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FileStatus {
    /// Its bytes are in the store: it's saved, uploaded and sent with the draft.
    #[default]
    Attached,
    /// Being read (from disk, or from a forwarded message): kept with the draft meanwhile,
    /// and taken up again after a restart.
    Attaching,
    /// It couldn't be: why, to show.
    Failed(String),
}

/// Where a file being attached comes from, kept with it so attaching it can go on after a
/// restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileSource {
    /// A file on this computer.
    Path(std::path::PathBuf),
    /// Attachment `index` of message `id` (a forward's), as [`Attachment::index`] numbers them.
    Message { id: MessageId, index: usize },
}

/// What the composer should say about a draft opened from the server.
#[derive(Clone, Debug, PartialEq)]
pub enum DraftNotice {
    /// It has a formatted (HTML) version: an edit keeps its text and attachments, not that.
    PlainText,
    /// It can't be edited in place without losing something (`why`): the composer has an
    /// unsaved copy, saved as a new draft, and the original stays in Drafts as it is.
    Copy(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Connecting,
    Syncing(String),
    Offline(String),
    Error(String),
}
