//! Orbit-owned SQLite storage.
//!
//! This module never opens wacli's databases. It stores immutable source
//! payloads and maintains an idempotent normalized projection with FTS5.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::{NormalizedMessage, SignalEntry};

#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    pub fn initialize(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.connect()?
            .execute_batch(SCHEMA)
            .context("initialize Orbit database")?;
        Ok(())
    }

    /// Insert the immutable payload and upsert its current message projection in
    /// one transaction. Re-delivery is safe due to the source identity key.
    pub fn ingest(
        &self,
        event_kind: &str,
        raw: &Value,
        message: &NormalizedMessage,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let inserted = ingest_transaction(&tx, event_kind, raw, message)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Persist a reconciliation page in one transaction. This keeps cursor
    /// catch-up fast while preserving the same event/projection atomicity as a
    /// single webhook delivery.
    pub fn ingest_batch(&self, messages: &[(&str, &Value, &NormalizedMessage)]) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut inserted = 0;
        for (event_kind, raw, message) in messages {
            inserted += usize::from(ingest_transaction(&tx, event_kind, raw, message)?);
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn search(&self, query: &str, limit: u32) -> Result<Value> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT m.external_id,m.chat_external_id,m.chat_name,m.sender_name,m.occurred_at,m.from_me,m.text,m.content_kind,m.filename,m.local_path
             FROM messages_fts f JOIN messages m ON m.id=f.rowid
             WHERE messages_fts MATCH ?1 ORDER BY bm25(messages_fts), m.occurred_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![sanitize_fts_query(query), limit], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?, "chat_jid": row.get::<_, String>(1)?,
                "chat_name": row.get::<_, String>(2)?, "sender_name": row.get::<_, String>(3)?,
                "timestamp": row.get::<_, String>(4)?, "from_me": row.get::<_, bool>(5)?,
                "text": row.get::<_, String>(6)?, "content_kind": row.get::<_, String>(7)?,
                "filename": row.get::<_, String>(8)?, "local_path": row.get::<_, String>(9)?
            }))
        })?;
        Ok(Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    pub fn stats(&self) -> Result<Value> {
        let conn = self.connect()?;
        let messages: i64 = conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?;
        let events: i64 = conn.query_row("SELECT count(*) FROM source_events", [], |r| r.get(0))?;
        let audited_actions: i64 =
            conn.query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))?;
        let last_event: Option<String> = conn
            .query_row("SELECT max(received_at) FROM source_events", [], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let bytes = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        Ok(
            json!({"messages": messages, "source_events": events, "audited_actions":audited_actions, "database_bytes": bytes, "last_event_at": last_event}),
        )
    }

    /// Return only the newest bounded projection rows needed by the TUI. This
    /// reads Orbit's own WAL-enabled database and never invokes the connector.
    pub fn signal_stream(&self, limit: u32) -> Result<Vec<SignalEntry>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT external_id,chat_external_id,chat_name,sender_name,occurred_at,text,content_kind,filename,from_me,revoked,raw_json
             FROM messages WHERE chat_external_id <> 'status@broadcast'
             ORDER BY occurred_at DESC,id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            let raw: String = row.get(10)?;
            let edited = serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                value
                    .get("Edited")
                    .or_else(|| value.get("edited"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            });
            Ok(SignalEntry {
                message_id: row.get(0)?,
                chat_jid: row.get(1)?,
                chat_name: row.get(2)?,
                sender_name: row.get(3)?,
                timestamp: row.get(4)?,
                text: row.get(5)?,
                content_kind: row.get(6)?,
                filename: row.get(7)?,
                from_me: row.get(8)?,
                revoked: row.get(9)?,
                edited,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read Signal Stream")
    }

    /// Record an operator-visible mutation without storing message bodies or
    /// attachment contents in the audit trail.
    pub fn record_action(
        &self,
        action: &str,
        target: &str,
        status: &str,
        details: &Value,
    ) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO audit_log(action,target,status,details_json) VALUES(?1,?2,?3,?4)",
            params![action, target, status, details.to_string()],
        )?;
        Ok(())
    }

    pub fn sync_cursor(&self, name: &str) -> Result<i64> {
        self.connect()?
            .query_row(
                "SELECT cursor_value FROM sync_cursors WHERE connector='whatsapp' AND cursor_name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0))
            .context("read reconciliation cursor")
    }

    pub fn set_sync_cursor(&self, name: &str, value: i64) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO sync_cursors(connector,cursor_name,cursor_value) VALUES('whatsapp',?1,?2)
             ON CONFLICT(connector,cursor_name) DO UPDATE SET cursor_value=max(cursor_value,excluded.cursor_value), updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![name, value],
        )?;
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn ingest_transaction(
    tx: &Transaction<'_>,
    event_kind: &str,
    raw: &Value,
    message: &NormalizedMessage,
) -> Result<bool> {
    // Hashing the canonical JSON distinguishes successive edits while
    // collapsing identical webhook and reconciliation deliveries.
    let payload_json = raw.to_string();
    let payload_hash = hex::encode(Sha256::digest(payload_json.as_bytes()));
    let inserted = tx.execute(
            "INSERT OR IGNORE INTO source_events(source, account_id, external_id, event_kind, occurred_at, payload_sha256, payload_json) VALUES('whatsapp', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![message.account_id, message.external_id, event_kind, message.timestamp, payload_hash, payload_json],
        )? == 1;
    // Live WhatsApp events may identify a chat by its LID while wacli's
    // durable row later uses the canonical phone-number JID. Prefer an
    // existing canonical row for LID delivery, and merge a prior LID row
    // when reconciliation supplies the canonical identity. WhatsApp
    // message IDs are the stable bridge between those two representations.
    let projection_chat = if message.chat_external_id.ends_with("@lid") {
        tx.query_row(
                "SELECT chat_external_id FROM messages WHERE account_id=?1 AND external_id=?2 AND chat_external_id NOT LIKE '%@lid' LIMIT 1",
                params![message.account_id, message.external_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| message.chat_external_id.clone())
    } else {
        tx.execute(
                "DELETE FROM messages WHERE account_id=?1 AND external_id=?2 AND chat_external_id LIKE '%@lid'",
                params![message.account_id, message.external_id],
            )?;
        message.chat_external_id.clone()
    };
    tx.execute(
            "INSERT INTO messages(account_id, chat_external_id, chat_name, external_id, sender_external_id, sender_name, occurred_at, from_me, text, content_kind, media_caption, filename, mime_type, local_path, revoked, raw_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(account_id, chat_external_id, external_id) DO UPDATE SET chat_name=excluded.chat_name, sender_external_id=excluded.sender_external_id, sender_name=excluded.sender_name, occurred_at=excluded.occurred_at, from_me=excluded.from_me, text=excluded.text, content_kind=excluded.content_kind, media_caption=excluded.media_caption, filename=excluded.filename, mime_type=excluded.mime_type, local_path=excluded.local_path, revoked=excluded.revoked, raw_json=excluded.raw_json, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![message.account_id, projection_chat, message.chat_name, message.external_id, message.sender_external_id, message.sender_name, message.timestamp, message.from_me, message.text, message.content_kind, message.media_caption, message.filename, message.mime_type, message.local_path, message.revoked, raw.to_string()],
        )?;
    Ok(inserted)
}

/// FTS operators are unnecessary for the first CLI and accepting them makes
/// malformed input surprising. Each token becomes a safely quoted term.
fn sanitize_fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS source_events (
  id INTEGER PRIMARY KEY, source TEXT NOT NULL, account_id TEXT NOT NULL,
  external_id TEXT NOT NULL, event_kind TEXT NOT NULL, occurred_at TEXT NOT NULL,
  received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  payload_sha256 TEXT NOT NULL,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  UNIQUE(source, account_id, external_id, event_kind, payload_sha256)
);
CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY, account_id TEXT NOT NULL, chat_external_id TEXT NOT NULL,
  chat_name TEXT NOT NULL DEFAULT '', external_id TEXT NOT NULL,
  sender_external_id TEXT NOT NULL DEFAULT '', sender_name TEXT NOT NULL DEFAULT '',
  occurred_at TEXT NOT NULL, from_me INTEGER NOT NULL CHECK(from_me IN (0,1)),
  text TEXT NOT NULL DEFAULT '', content_kind TEXT NOT NULL DEFAULT 'text',
  media_caption TEXT NOT NULL DEFAULT '', filename TEXT NOT NULL DEFAULT '',
  mime_type TEXT NOT NULL DEFAULT '', local_path TEXT NOT NULL DEFAULT '',
  revoked INTEGER NOT NULL DEFAULT 0 CHECK(revoked IN (0,1)), raw_json TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE(account_id, chat_external_id, external_id)
);
CREATE INDEX IF NOT EXISTS messages_time_idx ON messages(occurred_at DESC);
CREATE INDEX IF NOT EXISTS messages_chat_time_idx ON messages(chat_external_id, occurred_at DESC);
CREATE TABLE IF NOT EXISTS audit_log (
  id INTEGER PRIMARY KEY, occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  action TEXT NOT NULL, target TEXT NOT NULL, status TEXT NOT NULL,
  details_json TEXT NOT NULL CHECK(json_valid(details_json))
);
CREATE TABLE IF NOT EXISTS sync_cursors (
  connector TEXT NOT NULL, cursor_name TEXT NOT NULL, cursor_value INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  PRIMARY KEY(connector,cursor_name)
);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(text, media_caption, filename, chat_name, sender_name, content='messages', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid,text,media_caption,filename,chat_name,sender_name) VALUES(new.id,new.text,new.media_caption,new.filename,new.chat_name,new.sender_name);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts,rowid,text,media_caption,filename,chat_name,sender_name) VALUES('delete',old.id,old.text,old.media_caption,old.filename,old.chat_name,old.sender_name);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts,rowid,text,media_caption,filename,chat_name,sender_name) VALUES('delete',old.id,old.text,old.media_caption,old.filename,old.chat_name,old.sender_name);
  INSERT INTO messages_fts(rowid,text,media_caption,filename,chat_name,sender_name) VALUES(new.id,new.text,new.media_caption,new.filename,new.chat_name,new.sender_name);
END;
";

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> NormalizedMessage {
        NormalizedMessage {
            account_id: "personal".into(),
            chat_external_id: "1@s.whatsapp.net".into(),
            chat_name: "Alex".into(),
            external_id: "m1".into(),
            sender_external_id: "1@s.whatsapp.net".into(),
            sender_name: "Alex".into(),
            timestamp: "2026-08-25T10:00:00Z".into(),
            from_me: false,
            text: "launch proposal Monday".into(),
            content_kind: "text".into(),
            media_caption: String::new(),
            filename: String::new(),
            mime_type: String::new(),
            local_path: String::new(),
            edited: false,
            revoked: false,
        }
    }

    #[test]
    fn duplicate_delivery_keeps_one_event_and_message() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        assert!(
            store
                .ingest("message.created", &json!({"ID":"m1"}), &message())
                .unwrap()
        );
        assert!(
            !store
                .ingest("message.created", &json!({"ID":"m1"}), &message())
                .unwrap()
        );
        let stats = store.stats().unwrap();
        assert_eq!(stats["messages"], 1);
        assert_eq!(stats["source_events"], 1);
    }

    #[test]
    fn fts_search_finds_message_and_escapes_operators() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        store
            .ingest("message.created", &json!({"ID":"m1"}), &message())
            .unwrap();
        assert_eq!(
            store
                .search("launch", 10)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(store.search("launch OR", 10).is_ok());
    }

    #[test]
    fn distinct_edits_are_history_events_but_one_current_message() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        let mut current = message();
        current.text = "Friday".into();
        store
            .ingest(
                "message.created",
                &json!({"ID":"m1","Text":"Friday"}),
                &current,
            )
            .unwrap();
        current.text = "Monday".into();
        current.edited = true;
        store
            .ingest(
                "message.edited",
                &json!({"ID":"m1","Text":"Monday","Edited":true}),
                &current,
            )
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats["messages"], 1);
        assert_eq!(stats["source_events"], 2);
        assert_eq!(
            store
                .search("Monday", 10)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn reconciliation_merges_a_live_lid_into_its_canonical_chat() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        let mut live = message();
        live.chat_external_id = "123456@lid".into();
        store
            .ingest(
                "message.created",
                &json!({"ID":"m1","Chat":"123456@lid"}),
                &live,
            )
            .unwrap();
        let mut durable = live;
        durable.chat_external_id = "94770000000@s.whatsapp.net".into();
        store
            .ingest(
                "message.created",
                &json!({"MsgID":"m1","ChatJID":"94770000000@s.whatsapp.net"}),
                &durable,
            )
            .unwrap();

        let conn = store.connect().unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        let chat: String = conn
            .query_row("SELECT chat_external_id FROM messages", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(chat, "94770000000@s.whatsapp.net");
    }

    #[test]
    fn reconciliation_batch_is_atomic_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        let first = message();
        let mut second = message();
        second.external_id = "m2".into();
        second.text = "second message".into();
        let first_raw = json!({"ID":"m1"});
        let second_raw = json!({"ID":"m2"});
        let page = [
            ("message.created", &first_raw, &first),
            ("message.created", &second_raw, &second),
        ];

        assert_eq!(store.ingest_batch(&page).unwrap(), 2);
        assert_eq!(store.ingest_batch(&page).unwrap(), 0);
        assert_eq!(store.stats().unwrap()["messages"], 2);
    }

    #[test]
    fn signal_stream_is_newest_first_and_keeps_evidence_fields() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        let first = message();
        let mut newest = message();
        newest.external_id = "m2".into();
        newest.chat_external_id = "team@g.us".into();
        newest.chat_name = "Project Falcon".into();
        newest.sender_name = "Priya".into();
        newest.timestamp = "2026-08-26T09:42:00Z".into();
        newest.text = "Should we move the launch?".into();
        newest.edited = true;
        let mut status = newest.clone();
        status.external_id = "status".into();
        status.chat_external_id = "status@broadcast".into();
        status.timestamp = "2026-08-26T10:00:00Z".into();
        store
            .ingest("message.created", &json!({"ID":"m1"}), &first)
            .unwrap();
        store
            .ingest("message.edited", &json!({"ID":"m2","Edited":true}), &newest)
            .unwrap();
        store
            .ingest("message.created", &json!({"ID":"status"}), &status)
            .unwrap();

        let stream = store.signal_stream(20).unwrap();
        assert_eq!(stream[0].message_id, "m2");
        assert_eq!(stream[0].chat_jid, "team@g.us");
        assert_eq!(stream[0].chat_name, "Project Falcon");
        assert_eq!(stream[0].sender_name, "Priya");
        assert!(stream[0].edited);
        assert!(
            stream
                .iter()
                .all(|entry| entry.chat_jid != "status@broadcast")
        );
    }
}
