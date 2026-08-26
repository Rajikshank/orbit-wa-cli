//! Pinned `wacli` process adapter.
//!
//! All arguments are constructed as separate OS strings. No shell is involved,
//! which avoids command injection from contact names, messages, and file paths.

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use tokio::{
    process::{Child, Command},
    time::timeout,
};

use crate::{WACLI_VERSION, model::NormalizedMessage};

#[derive(Clone, Debug)]
pub struct Wacli {
    binary: PathBuf,
    store: PathBuf,
}

#[derive(Debug)]
pub struct ReconcileBatch {
    pub messages: Vec<(Value, NormalizedMessage)>,
    pub new_rows_count: usize,
    pub max_row_id: i64,
    pub max_mutation_timestamp: i64,
}

impl Wacli {
    #[must_use]
    pub fn new(binary: PathBuf, store: PathBuf) -> Self {
        Self { binary, store }
    }

    #[must_use]
    pub fn exists(&self) -> bool {
        self.binary.is_file()
    }

    pub async fn version(&self) -> Result<String> {
        let output = self
            .base_command()
            .arg("version")
            .output()
            .await
            .with_context(|| format!("start {}", self.binary.display()))?;
        if !output.status.success() {
            bail!("wacli version failed: {}", stderr(&output));
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(value)
    }

    pub async fn ensure_compatible(&self) -> Result<()> {
        if !self.exists() {
            bail!("wacli is not installed; run `orbit setup`");
        }
        let found = self.version().await?;
        if !found.contains(WACLI_VERSION) {
            bail!("unsupported wacli version `{found}`; Orbit requires {WACLI_VERSION}");
        }
        Ok(())
    }

    /// Authentication owns the terminal so the user can scan the QR code.
    pub async fn authenticate(&self) -> Result<()> {
        self.ensure_compatible().await?;
        let status = self
            // Pairing is the one driver operation that must inherit Orbit's
            // terminal. Every daemon-owned operation uses base_command
            // so Windows never flashes a transient console window.
            .interactive_command()
            .args(authentication_arguments())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("start wacli authentication")?;
        if !status.success() {
            bail!("WhatsApp authentication failed with {status}");
        }
        Ok(())
    }

    pub async fn run_json(&self, args: &[String]) -> Result<Value> {
        self.run_json_with_warning(args).await.map(|(data, _)| data)
    }

    /// Execute a driver command and preserve successful stderr warnings. Send
    /// warnings may mean WhatsApp accepted the message but local persistence
    /// failed, so callers must surface them and must never retry automatically.
    pub async fn run_json_with_warning(&self, args: &[String]) -> Result<(Value, Option<String>)> {
        self.ensure_compatible().await?;
        let mut command = self.base_command();
        command.arg("--json").arg("--timeout").arg("30s").args(args);
        let output = timeout(Duration::from_secs(40), command.output())
            .await
            .context("wacli command timed out")?
            .context("start wacli command")?;
        if !output.status.success() {
            bail!("wacli command failed: {}", stderr(&output));
        }
        let envelope: Value = serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "wacli returned incompatible JSON: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;
        let data = unwrap_envelope(&envelope)?;
        let warning = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Ok((data, (!warning.is_empty()).then_some(warning)))
    }

    pub fn spawn_sync(
        &self,
        webhook_url: &str,
        webhook_secret: &str,
        max_messages: u64,
        max_db_size: &str,
    ) -> Result<Child> {
        let child = self
            .base_command()
            .arg("sync")
            .arg("--follow")
            .arg("--events")
            .arg("--presence-mode")
            .arg("quiet")
            .arg("--max-reconnect")
            .arg("0")
            .arg("--max-messages")
            .arg(max_messages.to_string())
            .arg("--max-db-size")
            .arg(max_db_size)
            .arg("--webhook")
            .arg(webhook_url)
            .arg("--webhook-secret")
            .arg(webhook_secret)
            .arg("--webhook-allow-private")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start wacli sync")?;
        Ok(child)
    }

    pub async fn reconcile_messages(
        &self,
        after_row: i64,
        after_mutation: i64,
        limit: u32,
    ) -> Result<ReconcileBatch> {
        let database = self.store.join("wacli.db");
        if !database.is_file() {
            return Ok(ReconcileBatch {
                messages: Vec::new(),
                new_rows_count: 0,
                max_row_id: after_row,
                max_mutation_timestamp: after_mutation,
            });
        }
        tokio::task::spawn_blocking(move || {
            read_reconciliation_batch(&database, after_row, after_mutation, limit)
        })
        .await
        .context("reconciliation worker panicked")?
    }

    /// List contacts directly from the connector's read-only SQLite mirror.
    ///
    /// wacli 0.15 exposes contact search but deliberately rejects an empty
    /// query. Orbit's CLI promises that an omitted query lists contacts, so
    /// this narrow adapter fills that contract without taking wacli's writer
    /// lock or interrupting the long-running sync process.
    pub async fn list_contacts(&self, limit: u32) -> Result<Value> {
        let database = self.store.join("wacli.db");
        if !database.is_file() {
            return Ok(json!([]));
        }
        tokio::task::spawn_blocking(move || read_contact_list(&database, limit))
            .await
            .context("contact listing worker panicked")?
    }

    /// Resolve the connector's canonical chat JID for a message. Live webhook
    /// payloads can expose a LID while the durable media row is keyed by its
    /// phone JID, so download must not blindly reuse the displayed live ID.
    pub async fn resolve_download_chat(&self, message_id: &str, requested: &str) -> Result<String> {
        let database = self.store.join("wacli.db");
        let message_id = message_id.to_owned();
        let requested = requested.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &database,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("open {} read-only", database.display()))?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let mut statement =
                conn.prepare("SELECT DISTINCT chat_jid FROM messages WHERE msg_id=?1")?;
            let chats = statement
                .query_map([&message_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if chats.iter().any(|chat| chat == &requested) {
                return Ok(requested);
            }
            match chats.as_slice() {
                [only] => Ok(only.clone()),
                [] => bail!("message {message_id} was not found in the connector store"),
                _ => bail!(
                    "message {message_id} exists in multiple chats; pass its canonical chat JID"
                ),
            }
        })
        .await
        .context("download chat resolution worker panicked")?
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("--store").arg(&self.store);
        configure_background(&mut command);
        command
    }

    fn interactive_command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("--store").arg(&self.store);
        command
    }
}

/// A detached Windows daemon has no console to inherit. Without
/// `CREATE_NO_WINDOW`, every console-subsystem driver child receives a new
/// transient conhost window. Keep this flag away from interactive QR pairing.
#[cfg(windows)]
fn configure_background(command: &mut Command) {
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_background(_command: &mut Command) {}

/// `terminal` asks wacli to render an actual scannable QR. Its `text` mode is
/// intentionally the raw `https://wa.me/...` payload for external renderers and
/// must not be exposed as Orbit's interactive pairing experience.
fn authentication_arguments() -> [&'static str; 3] {
    ["auth", "--qr-format", "terminal"]
}

fn read_reconciliation_batch(
    database: &std::path::Path,
    after_row: i64,
    after_mutation: i64,
    limit: u32,
) -> Result<ReconcileBatch> {
    // Normal read-only mode (not immutable) is required to observe concurrent
    // WAL commits made by the long-running sync process.
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", database.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let sql = "SELECT rowid,chat_jid,coalesce(chat_name,''),msg_id,coalesce(sender_jid,''),coalesce(sender_name,''),ts,from_me,coalesce(text,''),coalesce(media_type,''),coalesce(media_caption,''),coalesce(filename,''),coalesce(mime_type,''),coalesce(local_path,''),revoked,edited,max(coalesce(edited_ts,0),coalesce(deleted_at,0)) AS mutation_ts FROM messages WHERE rowid>?1 ORDER BY rowid LIMIT ?2";
    let mut messages = query_reconciliation(&conn, sql, params![after_row, limit])?;
    let new_rows_count = messages.len();
    // Only the ordered new-row page may advance the row cursor. Mutation rows
    // are queried independently and can have a much higher rowid; including
    // them here would skip every unseen row between the page and that edit.
    let max_row_id = messages
        .iter()
        .map(|item| item.0)
        .max()
        .unwrap_or(after_row)
        .max(after_row);
    // Re-scan all mutations newer than the last completed second. The one-second
    // overlap avoids losing rows sharing a timestamp; payload hashes deduplicate.
    let mutation_floor = if after_mutation == 0 {
        0
    } else {
        after_mutation.saturating_sub(1)
    };
    let mutation_sql = "SELECT rowid,chat_jid,coalesce(chat_name,''),msg_id,coalesce(sender_jid,''),coalesce(sender_name,''),ts,from_me,coalesce(text,''),coalesce(media_type,''),coalesce(media_caption,''),coalesce(filename,''),coalesce(mime_type,''),coalesce(local_path,''),revoked,edited,max(coalesce(edited_ts,0),coalesce(deleted_at,0)) AS mutation_ts FROM messages WHERE edited_ts>?1 OR coalesce(deleted_at,0)>?1 ORDER BY mutation_ts,rowid";
    messages.extend(query_reconciliation(
        &conn,
        mutation_sql,
        params![mutation_floor],
    )?);
    let max_mutation_timestamp = messages
        .iter()
        .map(|item| item.1)
        .max()
        .unwrap_or(after_mutation)
        .max(after_mutation);
    Ok(ReconcileBatch {
        messages: messages
            .into_iter()
            .map(|(_, _, raw, msg)| (raw, msg))
            .collect(),
        new_rows_count,
        max_row_id,
        max_mutation_timestamp,
    })
}

fn read_contact_list(database: &std::path::Path, limit: u32) -> Result<Value> {
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", database.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut statement = conn.prepare(
        "SELECT c.jid,coalesce(c.phone,''),coalesce(a.alias,''),coalesce(c.system_name,''),coalesce(nullif(a.alias,''),nullif(c.system_name,''),nullif(c.full_name,''),nullif(c.push_name,''),nullif(c.business_name,''),nullif(c.first_name,''),''),c.updated_at FROM contacts c LEFT JOIN contact_aliases a ON a.jid=c.jid ORDER BY coalesce(nullif(a.alias,''),nullif(c.system_name,''),nullif(c.full_name,''),nullif(c.push_name,''),nullif(c.business_name,''),nullif(c.first_name,''),c.jid) LIMIT ?1",
    )?;
    let rows = statement.query_map(params![limit.max(1)], |row| {
        let updated_at: i64 = row.get(5)?;
        let updated_at = DateTime::<Utc>::from_timestamp(updated_at, 0)
            .map_or_else(|| updated_at.to_string(), |value| value.to_rfc3339());
        Ok(json!({
            "jid": row.get::<_, String>(0)?,
            "phone": row.get::<_, String>(1)?,
            "alias": row.get::<_, String>(2)?,
            "system_name": row.get::<_, String>(3)?,
            "name": row.get::<_, String>(4)?,
            "updated_at": updated_at,
        }))
    })?;
    Ok(Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?))
}

fn query_reconciliation<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<(i64, i64, Value, NormalizedMessage)>> {
    let mut statement = conn
        .prepare(sql)
        .context("prepare pinned wacli v0.15 reconciliation query")?;
    let rows = statement.query_map(params, |row| {
        let row_id:i64=row.get(0)?; let timestamp_seconds:i64=row.get(6)?; let mutation_ts:i64=row.get(16)?;
        let timestamp = DateTime::<Utc>::from_timestamp(timestamp_seconds,0).map_or_else(|| timestamp_seconds.to_string(), |value| value.to_rfc3339());
        let message = NormalizedMessage {
            account_id:"personal".into(), chat_external_id:row.get(1)?, chat_name:row.get(2)?, external_id:row.get(3)?,
            sender_external_id:row.get(4)?, sender_name:row.get(5)?, timestamp, from_me:row.get::<_,i64>(7)?!=0,
            text:row.get(8)?, content_kind:{let value:String=row.get(9)?;if value.is_empty(){"text".into()}else{value}},
            media_caption:row.get(10)?, filename:row.get(11)?, mime_type:row.get(12)?, local_path:row.get(13)?,
            revoked:row.get::<_,i64>(14)?!=0, edited:row.get::<_,i64>(15)?!=0,
        };
        let raw=json!({"ChatJID":message.chat_external_id,"ChatName":message.chat_name,"MsgID":message.external_id,"SenderJID":message.sender_external_id,"SenderName":message.sender_name,"Timestamp":message.timestamp,"FromMe":message.from_me,"Text":message.text,"MediaType":message.content_kind,"MediaCaption":message.media_caption,"Filename":message.filename,"MimeType":message.mime_type,"LocalPath":message.local_path,"Revoked":message.revoked,"Edited":message.edited});
        Ok((row_id,mutation_ts,raw,message))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("read pinned wacli v0.15 messages")
}

fn stderr(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        format!("exit status {}", output.status)
    } else {
        text
    }
}

/// v0.15.0 wraps every JSON response so process exit status and structured
/// command failure agree. Validate both instead of accidentally treating an
/// error envelope as useful command data.
fn unwrap_envelope(envelope: &Value) -> Result<Value> {
    let success = envelope
        .get("success")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("wacli JSON is missing boolean `success`"))?;
    if !success {
        let error = envelope
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown wacli error");
        bail!("{error}");
    }
    envelope
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow!("wacli JSON is missing `data`"))
}

/// Normalize CLI snake_case fields. Required identity fields fail closed so a
/// driver schema break cannot silently corrupt the local projection.
pub fn normalize_cli_message(raw: &Value) -> Result<NormalizedMessage> {
    normalize(raw, FieldStyle::Snake)
}

/// Normalize the live webhook's Go/PascalCase fields.
pub fn normalize_webhook_message(raw: &Value) -> Result<NormalizedMessage> {
    normalize(raw, FieldStyle::Pascal)
}

#[derive(Copy, Clone)]
enum FieldStyle {
    Snake,
    Pascal,
}

fn normalize(raw: &Value, style: FieldStyle) -> Result<NormalizedMessage> {
    let get = |snake: &str, pascal: &str| match style {
        FieldStyle::Snake => raw.get(snake),
        FieldStyle::Pascal => raw.get(pascal),
    };
    let string = |snake: &str, pascal: &str| {
        get(snake, pascal)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let external_id = string("MsgID", "ID");
    // wacli's Go Message lacks JSON tags for most fields, so its CLI output also
    // uses PascalCase today. Accept both shapes to tolerate that documented quirk.
    let external_id = if external_id.is_empty() {
        raw.get("external_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        external_id
    };
    let chat = string("ChatJID", "Chat");
    let chat = if chat.is_empty() {
        raw.get("chat_jid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        chat
    };
    if external_id.trim().is_empty() || chat.trim().is_empty() {
        bail!("message payload is missing message ID or chat JID: {raw}");
    }
    let media = get("Media", "Media");
    let media_string = |key: &str| {
        media
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let direct = |snake: &str, pascal: &str| {
        let v = string(snake, pascal);
        if v.is_empty() {
            raw.get(snake.to_ascii_lowercase())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        } else {
            v
        }
    };
    let media_type = {
        let direct_type = direct("MediaType", "MediaType");
        if direct_type.is_empty() {
            media_string("Type")
        } else {
            direct_type
        }
    };
    Ok(NormalizedMessage {
        account_id: "personal".into(),
        chat_external_id: chat,
        chat_name: direct("ChatName", "ChatName"),
        external_id,
        sender_external_id: direct("SenderJID", "SenderJID"),
        sender_name: direct("SenderName", "PushName"),
        timestamp: direct("Timestamp", "Timestamp"),
        from_me: get("FromMe", "FromMe")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text: direct("Text", "Text"),
        content_kind: if media_type.is_empty() {
            "text".into()
        } else {
            media_type.clone()
        },
        media_caption: {
            let v = direct("MediaCaption", "MediaCaption");
            if v.is_empty() {
                media_string("Caption")
            } else {
                v
            }
        },
        filename: {
            let v = direct("Filename", "Filename");
            if v.is_empty() {
                media_string("Filename")
            } else {
                v
            }
        },
        mime_type: {
            let v = direct("MimeType", "MimeType");
            if v.is_empty() {
                media_string("MimeType")
            } else {
                v
            }
        },
        local_path: direct("LocalPath", "LocalPath"),
        edited: get("Edited", "Edited")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        revoked: get("Revoked", "Revoked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[must_use]
pub fn command_result(kind: &str, value: &Value) -> Value {
    json!({"kind":kind,"result":value})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pinned_webhook_contract() {
        let raw = json!({"Chat":"123@s.whatsapp.net","ChatName":"Alex","ID":"m1","SenderJID":"123@s.whatsapp.net","Timestamp":"2026-08-25T10:00:00Z","FromMe":false,"Text":"hello","Media":{"Type":"image","Caption":"look","Filename":"a.jpg","MimeType":"image/jpeg"}});
        let msg = normalize_webhook_message(&raw).unwrap();
        assert_eq!(msg.external_id, "m1");
        assert_eq!(msg.content_kind, "image");
        assert_eq!(msg.media_caption, "look");
    }

    #[test]
    fn rejects_payload_without_stable_identity() {
        assert!(normalize_webhook_message(&json!({"Text":"hello"})).is_err());
    }

    #[test]
    fn validates_and_unwraps_v015_json_envelope() {
        assert_eq!(
            unwrap_envelope(&json!({"success":true,"data":{"messages":[]},"error":null})).unwrap(),
            json!({"messages":[]})
        );
        assert!(
            unwrap_envelope(&json!({"success":false,"data":null,"error":"not paired"})).is_err()
        );
    }

    #[test]
    fn interactive_auth_requests_a_scannable_terminal_qr() {
        assert_eq!(
            authentication_arguments(),
            ["auth", "--qr-format", "terminal"]
        );
    }

    #[tokio::test]
    async fn read_only_reconciliation_paginates_new_rows_and_recovers_edits() {
        let temp = tempfile::tempdir().unwrap();
        let connector_store = temp.path().join("connector");
        std::fs::create_dir_all(&connector_store).unwrap();
        let database = connector_store.join("wacli.db");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE messages(chat_jid TEXT,chat_name TEXT,msg_id TEXT,sender_jid TEXT,sender_name TEXT,ts INTEGER,from_me INTEGER,text TEXT,media_type TEXT,media_caption TEXT,filename TEXT,mime_type TEXT,local_path TEXT,revoked INTEGER,edited INTEGER,edited_ts INTEGER,deleted_at INTEGER);").unwrap();
        for id in 1..=3 {
            conn.execute("INSERT INTO messages VALUES('chat','Alex',?1,'sender','Alex',100,0,?2,'','','','','',0,0,0,NULL)", params![format!("m{id}"),format!("message {id}")]).unwrap();
        }
        // A mutation beyond the first page must not move the new-row cursor
        // from row 2 to row 3 and silently skip that third row.
        conn.execute(
            "UPDATE messages SET edited=1,edited_ts=50 WHERE msg_id='m3'",
            [],
        )
        .unwrap();
        drop(conn);
        let driver = Wacli::new(PathBuf::from("unused"), connector_store);
        let first = driver.reconcile_messages(0, 0, 2).await.unwrap();
        assert_eq!(first.new_rows_count, 2);
        assert_eq!(first.max_row_id, 2);
        assert!(
            first
                .messages
                .iter()
                .any(|(_, message)| message.external_id == "m3")
        );
        let second = driver
            .reconcile_messages(first.max_row_id, 0, 2)
            .await
            .unwrap();
        assert_eq!(second.new_rows_count, 1);
        assert_eq!(second.max_row_id, 3);

        let conn = Connection::open(&database).unwrap();
        conn.execute(
            "UPDATE messages SET text='edited value',edited=1,edited_ts=200 WHERE msg_id='m1'",
            [],
        )
        .unwrap();
        drop(conn);
        let mutation = driver.reconcile_messages(3, 0, 2).await.unwrap();
        assert_eq!(mutation.new_rows_count, 0);
        assert!(
            mutation
                .messages
                .iter()
                .any(|(_, message)| message.external_id == "m1" && message.text == "edited value")
        );
        assert_eq!(mutation.max_mutation_timestamp, 200);
    }

    #[tokio::test]
    async fn omitted_contact_query_lists_contacts_without_the_writer_lock() {
        let temp = tempfile::tempdir().unwrap();
        let connector_store = temp.path().join("connector");
        std::fs::create_dir_all(&connector_store).unwrap();
        let database = connector_store.join("wacli.db");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE contacts(jid TEXT PRIMARY KEY,phone TEXT,system_name TEXT,full_name TEXT,push_name TEXT,business_name TEXT,first_name TEXT,updated_at INTEGER); CREATE TABLE contact_aliases(jid TEXT PRIMARY KEY,alias TEXT);").unwrap();
        conn.execute(
            "INSERT INTO contacts VALUES('2@s.whatsapp.net','2','','Zulu','','','',200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contacts VALUES('1@s.whatsapp.net','1','','Alpha','','','',100)",
            [],
        )
        .unwrap();
        drop(conn);

        let driver = Wacli::new(PathBuf::from("unused"), connector_store);
        let contacts = driver.list_contacts(1).await.unwrap();
        assert_eq!(contacts.as_array().unwrap().len(), 1);
        assert_eq!(contacts[0]["name"], "Alpha");
        assert_eq!(contacts[0]["jid"], "1@s.whatsapp.net");
    }

    #[tokio::test]
    async fn media_download_resolves_a_live_lid_to_the_durable_chat() {
        let temp = tempfile::tempdir().unwrap();
        let connector_store = temp.path().join("connector");
        std::fs::create_dir_all(&connector_store).unwrap();
        let database = connector_store.join("wacli.db");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE messages(chat_jid TEXT,msg_id TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO messages VALUES('94770000000@s.whatsapp.net','m1')",
            [],
        )
        .unwrap();
        drop(conn);

        let driver = Wacli::new(PathBuf::from("unused"), connector_store);
        assert_eq!(
            driver
                .resolve_download_chat("m1", "123456@lid")
                .await
                .unwrap(),
            "94770000000@s.whatsapp.net"
        );
    }
}
