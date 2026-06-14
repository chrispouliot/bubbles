//! SQLite-backed message store (Phase B).
//!
//! All logic lives in the sync `*_blocking` / `query_*` functions over a
//! `rusqlite::Connection`; they are unit-tested directly (see `#[cfg(test)]`).
//! The async [`Store`] is a thin `tokio-rusqlite` wrapper used by the app, so
//! DB work happens off the GTK thread and bridges back the same way the rest of
//! the app does.
//!
//! Design choices come straight from the receive spike:
//!   * messages fan out to multiple destinations, so inserts dedupe on `guid`;
//!   * read/delivered receipts reuse the target's guid and carry no chat, so
//!     they're UPDATEs, not inserts;
//!   * a chat is identified by its sorted participant set (self included).

mod model;
pub use model::*;

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};

const SCHEMA_VERSION: i64 = 1;

const DDL: &str = "
CREATE TABLE handle(
  id      INTEGER PRIMARY KEY,
  address TEXT UNIQUE NOT NULL,
  is_me   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE chat(
  id                INTEGER PRIMARY KEY,
  key               TEXT UNIQUE NOT NULL,
  display_name      TEXT,
  is_group          INTEGER NOT NULL DEFAULT 0,
  service           TEXT,
  last_message_date INTEGER
);
CREATE TABLE chat_participant(
  chat_id   INTEGER NOT NULL REFERENCES chat(id)   ON DELETE CASCADE,
  handle_id INTEGER NOT NULL REFERENCES handle(id) ON DELETE CASCADE,
  PRIMARY KEY(chat_id, handle_id)
);
CREATE TABLE message(
  id               INTEGER PRIMARY KEY,
  guid             TEXT UNIQUE NOT NULL,
  chat_id          INTEGER NOT NULL REFERENCES chat(id) ON DELETE CASCADE,
  sender_handle_id INTEGER REFERENCES handle(id),
  is_from_me       INTEGER NOT NULL DEFAULT 0,
  text             TEXT,
  subject          TEXT,
  service          TEXT,
  date             INTEGER NOT NULL,
  date_delivered   INTEGER,
  date_read        INTEGER,
  effect           TEXT,
  reply_to_guid    TEXT,
  reply_part       TEXT,
  associated_guid  TEXT,
  associated_type  INTEGER,
  item_type        INTEGER NOT NULL DEFAULT 0,
  error            INTEGER
);
CREATE INDEX idx_message_chat_date ON message(chat_id, date);
CREATE INDEX idx_message_assoc     ON message(associated_guid);
CREATE TABLE attachment(
  id            INTEGER PRIMARY KEY,
  message_id    INTEGER NOT NULL REFERENCES message(id) ON DELETE CASCADE,
  guid          TEXT,
  mime_type     TEXT,
  transfer_name TEXT,
  total_bytes   INTEGER,
  local_path    TEXT,
  part_index    INTEGER,
  is_sticker    INTEGER NOT NULL DEFAULT 0
);
";

// --- sync core (all logic; unit-tested) ---

/// Apply pending migrations and enable FK enforcement on this connection.
pub fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "foreign_keys", true)?;
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < SCHEMA_VERSION {
        c.execute_batch(DDL)?;
        c.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn upsert_handle(c: &Connection, address: &str, is_me: bool) -> rusqlite::Result<i64> {
    let addr = address.to_lowercase();
    // `is_me` is sticky: once a handle is known to be ours it stays that way.
    c.execute(
        "INSERT INTO handle(address, is_me) VALUES (?1, ?2)
         ON CONFLICT(address) DO UPDATE SET is_me = handle.is_me | excluded.is_me",
        params![addr, is_me as i64],
    )?;
    c.query_row("SELECT id FROM handle WHERE address = ?1", params![addr], |r| r.get(0))
}

fn upsert_chat(c: &Connection, chat: &ChatRef) -> rusqlite::Result<i64> {
    let key = chat.key();
    // Fill in a name/service we didn't have before, but never clobber with NULL.
    c.execute(
        "INSERT INTO chat(key, display_name, is_group, service) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET
            display_name = COALESCE(excluded.display_name, chat.display_name),
            service      = COALESCE(excluded.service, chat.service)",
        params![key, chat.display_name, chat.is_group() as i64, chat.service],
    )?;
    let chat_id: i64 =
        c.query_row("SELECT id FROM chat WHERE key = ?1", params![key], |r| r.get(0))?;
    for p in &chat.participants {
        let hid = upsert_handle(c, p, false)?;
        c.execute(
            "INSERT OR IGNORE INTO chat_participant(chat_id, handle_id) VALUES (?1, ?2)",
            params![chat_id, hid],
        )?;
    }
    Ok(chat_id)
}

fn bump_chat_date(c: &Connection, chat_id: i64, date: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE chat SET last_message_date = MAX(COALESCE(last_message_date, 0), ?2)
         WHERE id = ?1",
        params![chat_id, date],
    )?;
    Ok(())
}

fn insert_message(c: &Connection, m: &IncomingMessage) -> rusqlite::Result<()> {
    let chat_id = upsert_chat(c, &m.chat)?;
    let sender_id = match &m.sender {
        Some(a) => Some(upsert_handle(c, a, m.is_from_me)?),
        None => None,
    };
    c.execute(
        "INSERT OR IGNORE INTO message
           (guid, chat_id, sender_handle_id, is_from_me, text, subject, service, date,
            effect, reply_to_guid, reply_part, item_type)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            m.guid, chat_id, sender_id, m.is_from_me as i64, m.text, m.subject, m.service,
            m.date, m.effect, m.reply_to_guid, m.reply_part, m.item_type
        ],
    )?;
    bump_chat_date(c, chat_id, m.date)
}

fn insert_tapback(c: &Connection, t: &Tapback) -> rusqlite::Result<()> {
    let chat_id = upsert_chat(c, &t.chat)?;
    let sender_id = match &t.sender {
        Some(a) => Some(upsert_handle(c, a, t.is_from_me)?),
        None => None,
    };
    c.execute(
        "INSERT OR IGNORE INTO message
           (guid, chat_id, sender_handle_id, is_from_me, date,
            associated_guid, associated_type, reply_part, item_type)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)",
        params![
            t.guid, chat_id, sender_id, t.is_from_me as i64, t.date,
            t.associated_guid, t.associated_type, t.associated_part
        ],
    )?;
    bump_chat_date(c, chat_id, t.date)
}

/// Apply one inbound event in a transaction.
pub fn apply_blocking(c: &mut Connection, ingest: Ingest) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    match ingest {
        Ingest::Message(m) => insert_message(&tx, &m)?,
        Ingest::Tapback(t) => insert_tapback(&tx, &t)?,
        Ingest::Receipt(Receipt::Delivered { guid, date }) => {
            tx.execute(
                "UPDATE message SET date_delivered = ?1
                 WHERE guid = ?2 AND date_delivered IS NULL",
                params![date, guid],
            )?;
        }
        Ingest::Receipt(Receipt::Read { guid, date }) => {
            tx.execute(
                "UPDATE message SET date_read = ?1
                 WHERE guid = ?2 AND date_read IS NULL",
                params![date, guid],
            )?;
        }
        Ingest::Ignored(_) => {}
    }
    tx.commit()
}

pub fn query_chats(c: &Connection) -> rusqlite::Result<Vec<ChatSummary>> {
    let mut stmt = c.prepare(
        "SELECT c.id, c.key, c.display_name, c.is_group, c.service, c.last_message_date,
                COALESCE(GROUP_CONCAT(h.address, ';'), '')
         FROM chat c
         LEFT JOIN chat_participant cp ON cp.chat_id = c.id
         LEFT JOIN handle h            ON h.id = cp.handle_id
         GROUP BY c.id
         ORDER BY (c.last_message_date IS NULL), c.last_message_date DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let parts: String = r.get(6)?;
        Ok(ChatSummary {
            id: r.get(0)?,
            key: r.get(1)?,
            display_name: r.get(2)?,
            is_group: r.get::<_, i64>(3)? != 0,
            service: r.get(4)?,
            last_message_date: r.get(5)?,
            participants: if parts.is_empty() {
                vec![]
            } else {
                parts.split(';').map(String::from).collect()
            },
        })
    })?;
    rows.collect()
}

pub fn query_messages(c: &Connection, chat_id: i64) -> rusqlite::Result<Vec<StoredMessage>> {
    let mut stmt = c.prepare(
        "SELECT m.id, m.guid, m.chat_id, h.address, m.is_from_me, m.text, m.subject, m.service,
                m.date, m.date_delivered, m.date_read, m.effect, m.reply_to_guid, m.reply_part,
                m.associated_guid, m.associated_type, m.item_type
         FROM message m LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.chat_id = ?1
         ORDER BY m.date ASC, m.id ASC",
    )?;
    let rows = stmt.query_map(params![chat_id], |r| {
        Ok(StoredMessage {
            id: r.get(0)?,
            guid: r.get(1)?,
            chat_id: r.get(2)?,
            sender: r.get(3)?,
            is_from_me: r.get::<_, i64>(4)? != 0,
            text: r.get(5)?,
            subject: r.get(6)?,
            service: r.get(7)?,
            date: r.get(8)?,
            date_delivered: r.get(9)?,
            date_read: r.get(10)?,
            effect: r.get(11)?,
            reply_to_guid: r.get(12)?,
            reply_part: r.get(13)?,
            associated_guid: r.get(14)?,
            associated_type: r.get(15)?,
            item_type: r.get(16)?,
        })
    })?;
    rows.collect()
}

// --- async wrapper (used by the app) ---

/// Async handle to the message database. Cloneable; all clones share the one
/// underlying connection/thread.
#[derive(Clone)]
pub struct Store {
    conn: tokio_rusqlite::Connection,
}

impl Store {
    /// Open (creating if needed) the DB at `path` and run migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = tokio_rusqlite::Connection::open(path.as_ref().to_owned()).await?;
        conn.call(|c| {
            migrate(c)?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    /// Open an in-memory DB (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self> {
        let conn = tokio_rusqlite::Connection::open_in_memory().await?;
        conn.call(|c| {
            migrate(c)?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn apply(&self, ingest: Ingest) -> Result<()> {
        self.conn
            .call(move |c| {
                apply_blocking(c, ingest)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn chats(&self) -> Result<Vec<ChatSummary>> {
        Ok(self.conn.call(|c| Ok(query_chats(c)?)).await?)
    }

    pub async fn messages(&self, chat_id: i64) -> Result<Vec<StoredMessage>> {
        Ok(self.conn.call(move |c| Ok(query_messages(c, chat_id)?)).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn chat_1to1() -> ChatRef {
        ChatRef {
            participants: vec![
                "mailto:chrispouliot@icloud.com".into(),
                "mailto:asd@icloud.com".into(),
            ],
            display_name: None,
            service: Some("iMessage".into()),
        }
    }

    fn msg(guid: &str, date: i64) -> IncomingMessage {
        IncomingMessage {
            guid: guid.into(),
            chat: chat_1to1(),
            sender: Some("mailto:asd@icloud.com".into()),
            is_from_me: false,
            text: Some("Hello me".into()),
            date,
            ..Default::default()
        }
    }

    #[test]
    fn dedupes_duplicate_delivery() {
        let mut c = db();
        // Same guid twice (the fan-out duplicate seen in the spike).
        apply_blocking(&mut c, Ingest::Message(msg("G1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("G1", 1000))).unwrap();

        let chats = query_chats(&c).unwrap();
        assert_eq!(chats.len(), 1, "one chat");
        assert_eq!(chats[0].is_group, false);
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "duplicate guid collapsed to one row");
        assert_eq!(msgs[0].text.as_deref(), Some("Hello me"));
        assert_eq!(chats[0].last_message_date, Some(1000));
        assert_eq!(chats[0].participants.len(), 2);
    }

    #[test]
    fn receipt_updates_existing_message() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("G2", 2000))).unwrap();
        // Receipt reuses the target guid, carries no chat.
        apply_blocking(&mut c, Ingest::Receipt(Receipt::Read { guid: "G2".into(), date: 2500 }))
            .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "receipt did not insert a new row");
        assert_eq!(msgs[0].date_read, Some(2500));
        assert_eq!(msgs[0].date_delivered, None);
    }

    #[test]
    fn receipt_for_unknown_guid_is_noop() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Receipt(Receipt::Delivered { guid: "nope".into(), date: 1 }))
            .unwrap();
        assert!(query_chats(&c).unwrap().is_empty());
    }

    #[test]
    fn distinct_participant_sets_are_distinct_chats() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("A", 10))).unwrap();

        let mut m = msg("B", 20);
        m.chat = ChatRef {
            participants: vec!["tel:+15551234".into(), "mailto:asd@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        apply_blocking(&mut c, Ingest::Message(m)).unwrap();

        assert_eq!(query_chats(&c).unwrap().len(), 2, "different sets -> different chats");
    }

    #[test]
    fn key_is_order_and_case_insensitive() {
        let a = ChatRef {
            participants: vec!["mailto:B@x.com".into(), "mailto:a@x.com".into()],
            ..Default::default()
        };
        let b = ChatRef {
            participants: vec!["mailto:a@x.com".into(), "mailto:b@x.com".into()],
            ..Default::default()
        };
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn group_detection() {
        let g = ChatRef {
            participants: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert!(g.is_group());
    }

    #[test]
    fn tapback_stored_with_association() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("T1", 100))).unwrap();
        apply_blocking(
            &mut c,
            Ingest::Tapback(Tapback {
                guid: "R1".into(),
                chat: chat_1to1(),
                sender: Some("mailto:asd@icloud.com".into()),
                is_from_me: false,
                date: 150,
                associated_guid: "T1".into(),
                associated_part: Some("0".into()),
                associated_type: 2000, // Heart, add
            }),
        )
        .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 2);
        let react = msgs.iter().find(|m| m.guid == "R1").unwrap();
        assert_eq!(react.associated_guid.as_deref(), Some("T1"));
        assert_eq!(react.associated_type, Some(2000));
    }
}
