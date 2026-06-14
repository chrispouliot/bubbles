//! Backend-agnostic data the [`super::Store`] ingests and returns. Deliberately
//! free of rustpush and GTK types, so the store layer builds and is unit-tested
//! on its own. The rustpush -> [`Ingest`] mapping lives in the backend layer
//! (`protocol::rustpush_backend::ingest_from`), keeping this module portable.

/// A chat referenced by its participant set (full `tel:`/`mailto:` addresses).
#[derive(Clone, Debug, Default)]
pub struct ChatRef {
    /// All members, *including* self, exactly as received.
    pub participants: Vec<String>,
    /// Group name (`cv_name`); `None` for 1:1s.
    pub display_name: Option<String>,
    /// "iMessage" / "SMS", when known.
    pub service: Option<String>,
}

impl ChatRef {
    /// Normalized participant list: lowercased, de-duped, sorted.
    fn normalized(&self) -> Vec<String> {
        let mut p: Vec<String> = self.participants.iter().map(|s| s.to_lowercase()).collect();
        p.sort();
        p.dedup();
        p
    }

    /// Stable identity key — what distinguishes one conversation from another.
    /// Includes self, so threads with different membership stay separate.
    pub fn key(&self) -> String {
        self.normalized().join(";")
    }

    /// `participants` includes self, so >2 distinct members is a group.
    pub fn is_group(&self) -> bool {
        self.normalized().len() > 2
    }
}

/// A content message to insert. Idempotent on `guid`.
#[derive(Clone, Debug, Default)]
pub struct IncomingMessage {
    pub guid: String,
    pub chat: ChatRef,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub service: Option<String>,
    /// Unix epoch milliseconds.
    pub date: i64,
    pub effect: Option<String>,
    pub reply_to_guid: Option<String>,
    pub reply_part: Option<String>,
    /// 0 = normal text; non-zero reserved for rename/participant-change/etc.
    pub item_type: i64,
}

/// A delivery/read receipt updating an existing message (referenced by its guid).
#[derive(Clone, Debug)]
pub enum Receipt {
    Delivered { guid: String, date: i64 },
    Read { guid: String, date: i64 },
}

/// A tapback/reaction. Stored as a message row carrying `associated_*`.
#[derive(Clone, Debug, Default)]
pub struct Tapback {
    /// The reaction message's own guid (for dedupe).
    pub guid: String,
    pub chat: ChatRef,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub date: i64,
    /// Target message guid.
    pub associated_guid: String,
    pub associated_part: Option<String>,
    /// Apple tapback code: 2000-2005 add, 3000-3005 remove.
    pub associated_type: i64,
}

/// One inbound event — the single thing the store ingests.
#[derive(Clone, Debug)]
pub enum Ingest {
    Message(IncomingMessage),
    Receipt(Receipt),
    Tapback(Tapback),
    /// A recognized-but-unstored control event; the &str names the variant.
    Ignored(&'static str),
}

// --- read side ---

#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub id: i64,
    pub key: String,
    pub display_name: Option<String>,
    pub is_group: bool,
    pub service: Option<String>,
    pub last_message_date: Option<i64>,
    pub participants: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub id: i64,
    pub guid: String,
    pub chat_id: i64,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub service: Option<String>,
    pub date: i64,
    pub date_delivered: Option<i64>,
    pub date_read: Option<i64>,
    pub effect: Option<String>,
    pub reply_to_guid: Option<String>,
    pub reply_part: Option<String>,
    pub associated_guid: Option<String>,
    pub associated_type: Option<i64>,
    pub item_type: i64,
}
