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
    /// The HTML turned into Markdown, for drawing natively.
    pub markdown: Option<String>,
    pub attachments: Vec<Attachment>,
}

/// One message of an opened conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMessage {
    pub summary: Summary,
    pub body: Option<Body>,
}

#[derive(Clone, Debug, PartialEq, Default)]
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
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Connecting,
    Syncing(String),
    Offline(String),
    Error(String),
}
