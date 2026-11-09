//! Types shared by the engine and the UI.

use serde::{Deserialize, Serialize};

pub type MessageId = i64;

pub mod flags {
    pub const SEEN: u32 = 1;
    pub const FLAGGED: u32 = 2;
    pub const ANSWERED: u32 = 4;
    pub const DRAFT: u32 = 8;
    pub const DELETED: u32 = 16;
    /// Back from a snooze, and not dealt with yet: pinned to the top of the inbox. The server's
    /// `$new` keyword where it keeps keywords; elsewhere this computer's alone
    /// ([`crate::snooze`]).
    pub const WOKEN: u32 = 32;
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
    /// A snooze bucket ([`crate::snooze`]), its parent, or the server's own snoozed mailbox.
    Snoozed,
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
            Role::Snoozed => "snoozed",
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
            "snoozed" => Role::Snoozed,
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
            Role::Snoozed => "Snoozed",
            Role::Other => "",
        }
    }

    /// Sidebar order.
    pub fn rank(self) -> u8 {
        match self {
            Role::Inbox => 0,
            Role::Flagged => 1,
            Role::Snoozed => 2,
            Role::Important => 3,
            Role::Sent => 4,
            Role::Drafts => 5,
            Role::Archive => 6,
            Role::All => 7,
            Role::Junk => 8,
            Role::Trash => 9,
            Role::Other => 10,
        }
    }
}

/// Where a conversation goes in the split inbox: decided here, from its headers and the user's
/// rules (see `category.rs`), never on the server. Written as [`Category::as_str`] names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Category {
    /// Mail from people that isn't for you in particular: copied, or sent to a group.
    #[default]
    Other,
    Newsletters,
    Notifications,
    /// Mail from people you write to, sent to you, or in a conversation you're part of.
    Important,
}

impl Category {
    /// In the order the inbox shows them.
    pub const ALL: [Category; 4] = [Category::Important, Category::Other, Category::Newsletters, Category::Notifications];

    /// As stored (`messages.category`). Other is 0, the column's default: a message that hasn't
    /// been sorted yet shows in Other, never nowhere.
    pub fn code(self) -> i64 {
        match self {
            Category::Other => 0,
            Category::Newsletters => 1,
            Category::Notifications => 2,
            Category::Important => 3,
        }
    }

    pub fn from_code(code: i64) -> Category {
        match code {
            1 => Category::Newsletters,
            2 => Category::Notifications,
            3 => Category::Important,
            _ => Category::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Important => "important",
            Category::Other => "other",
            Category::Newsletters => "newsletters",
            Category::Notifications => "notifications",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Important => "Important",
            Category::Other => "Other",
            Category::Newsletters => "Newsletters",
            Category::Notifications => "Notifications",
        }
    }

    /// A category's name as typed (`newsletters`, `Newsletter`), any case.
    pub fn parse(s: &str) -> Option<Category> {
        let s = s.trim().to_lowercase();
        let s = s.strip_suffix('s').unwrap_or(&s);
        Category::ALL.into_iter().find(|c| c.as_str().strip_suffix('s').unwrap_or(c.as_str()) == s)
    }

    /// Which wins when a conversation's messages disagree: a person over a machine, and you
    /// over anyone.
    pub fn strength(self) -> u8 {
        match self {
            Category::Newsletters => 0,
            Category::Notifications => 1,
            Category::Other => 2,
            Category::Important => 3,
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
    /// Envoy's snooze bucket: when its messages wake (Unix seconds). None for every other
    /// mailbox, the server's own snoozed one too ([`crate::snooze`]).
    pub wake: Option<i64>,
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

    /// As written into a recipients field, to be sent as it is: `Name <address>`, the name in
    /// quotes when it has a character that would split the field or not parse bare (a comma,
    /// brackets, `@`), or the address alone.
    pub fn recipient(&self) -> String {
        let name: String = self.name.trim().chars().filter(|c| !matches!(c, '"' | '\\')).collect();
        if name.is_empty() {
            return self.email.clone();
        }
        let bare = !name.starts_with('.') && name.chars().all(|c| !c.is_ascii() || c.is_ascii_alphanumeric() || c == ' ' || "!#$%&'*+-/=?^_`{|}~.".contains(c));
        if bare { format!("{name} <{}>", self.email) } else { format!("\"{name}\" <{}>", self.email) }
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
    /// Its conversation's category (the same for every message of it).
    pub category: Category,
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
    /// It was sent, and couldn't go (`why`: its server won't take it): back to be changed.
    NotSent(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Connecting,
    Syncing(String),
    Offline(String),
    Error(String),
}
