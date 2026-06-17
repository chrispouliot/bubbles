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
    /// Files attached to this message (already downloaded to `local_path`).
    pub attachments: Vec<AttachmentRecord>,
}

/// An attachment as ingested: metadata plus a local file we've already saved.
#[derive(Clone, Debug, Default)]
pub struct AttachmentRecord {
    pub guid: Option<String>,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub total_bytes: Option<i64>,
    pub local_path: Option<String>,
    pub part_index: Option<i64>,
    pub is_sticker: bool,
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
    /// A sender-supplied link preview (iMessage rich link). Persisted in
    /// `message_link_preview`, upserted on `(message_guid, part_idx)`.
    LinkPreview(MessageLinkPreview),
    /// A recognized-but-unstored control event; the &str names the variant.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub last_message_date: Option<i64>,
    pub participants: Vec<String>,
    pub unread: i64,
    /// User-set name overriding the derived title; `None` falls back to the
    /// Apple group name or the participants.
    pub custom_name: Option<String>,
}

/// A freshly-arrived inbound message, used to drive desktop notifications.
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub chat_id: i64,
    pub sender: Option<String>,
    pub text: Option<String>,
    pub has_attachment: bool,
    pub date: i64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
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
    pub attachments: Vec<StoredAttachment>,
}

/// An attachment as read back for display.
#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub mime: Option<String>,
    pub name: Option<String>,
    pub local_path: Option<String>,
    #[allow(dead_code)]
    pub is_sticker: bool,
}

impl StoredAttachment {
    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .map_or(false, |m| m.starts_with("image/"))
    }
}

/// A sender-generated URL preview, attached to a specific message. This is the
/// iMessage rich-link / LinkPresentation data the sender's device shipped to us:
/// title, summary, URL, and the inline thumbnail bytes rustpush already pulled
/// from the balloon body. We do not fetch incoming URLs; this is the sender's
/// static snapshot, keyed by message.
///
/// `part_idx` distinguishes multiple links on the same message (rare; iMessage
/// typically carries one URL per message, but the schema allows more). The
/// `(message_guid, part_idx)` pair is the primary key so a placeholder can be
/// upserted in place when the fill-in arrives.
#[derive(Clone, Debug)]
pub struct MessageLinkPreview {
    pub message_guid: String,
    pub part_idx: i64,
    /// Canonical (post-redirect) URL the sender's device resolved to.
    pub url: Option<String>,
    /// Whatever the sender actually typed; preserved when it differs from `url`.
    pub original_url: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    /// Local cache path of the thumbnail (`$XDG_CACHE_HOME/.../previews/...`).
    /// `None` when the sender didn't include one.
    pub image_path: Option<String>,
    /// Width/height of `image_path` (best-effort, used for sizing).
    pub image_width: Option<i64>,
    pub image_height: Option<i64>,
    /// `is_incomplete == true` from the LPLinkMetadata — Apple sent a
    /// placeholder balloon, the real preview is on its way. Render the
    /// compact "loading…" state and replace in place when the fill-in arrives.
    pub is_placeholder: bool,
}

impl MessageLinkPreview {
    /// True when the title *and* summary are both empty (placeholder preview or
    /// a sender-supplied empty card). The renderer collapses such cards into a
    /// compact "loading preview…" state instead of an empty shell.
    pub fn is_sparse(&self) -> bool {
        let title_blank = self
            .title
            .as_deref()
            .map_or(true, |s| s.trim().is_empty());
        let summary_blank = self
            .summary
            .as_deref()
            .map_or(true, |s| s.trim().is_empty());
        title_blank && summary_blank
    }
}

/// A URL-keyed, fetched preview. Distinct from [`MessageLinkPreview`]: that one
/// is message-scoped, sender-supplied, and immutable. This one is keyed by URL
/// (so re-opening the same link reuses the result), TTL'd, and the result of
/// *us* fetching the page (Phase 5+). For now Phase 1-3 doesn't write to this
/// table, but the schema lives alongside `message_link_preview` in the same
/// migration so the rollout is one atomic bump.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct LinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    pub image_path: Option<String>,
    /// Unix epoch ms when the row was last refreshed.
    pub fetched_at: i64,
    /// 0 = ok, 1 = failed (so a 404 isn't re-hit every render).
    pub status: i64,
    pub error: Option<String>,
}
