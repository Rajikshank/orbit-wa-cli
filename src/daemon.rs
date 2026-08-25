//! Long-running daemon orchestration and command policy.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rand::RngCore;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex, watch},
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    config::{Config, OrbitPaths},
    ipc::{self, Handler},
    model::{Request, Response},
    store::Store,
    wacli::{Wacli, command_result},
    webhook,
};

#[derive(Clone)]
struct DaemonState {
    wacli: Wacli,
    store: Store,
    sync_running: Arc<AtomicBool>,
    sync_pause: watch::Sender<bool>,
    connector_operation: Arc<Mutex<()>>,
    shutdown: watch::Sender<bool>,
}

/// Reset the pause flag even when an exclusive connector command fails or its
/// request future is cancelled. The supervisor can then resume normal sync.
struct SyncPauseGuard {
    sender: watch::Sender<bool>,
}

impl SyncPauseGuard {
    fn activate(sender: watch::Sender<bool>) -> Result<Self> {
        sender
            .send(true)
            .map_err(|_| anyhow!("WhatsApp sync supervisor is unavailable"))?;
        Ok(Self { sender })
    }
}

impl Drop for SyncPauseGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(false);
    }
}

impl DaemonState {
    async fn handle(&self, request: Request) -> Response {
        match request {
            Request::SendText { to, message } => return self.send_text(to, message).await,
            Request::SendFile {
                to,
                path,
                caption,
                media_as,
                voice,
            } => return self.send_file(to, path, caption, media_as, voice).await,
            other => match self.handle_result(other).await {
                Ok(value) => Response::success(value),
                Err(error) => Response::failure(format!("{error:#}")),
            },
        }
    }

    async fn send_text(&self, to: String, message: String) -> Response {
        self.audited_send(
            "whatsapp.send_text",
            &to,
            &[
                "send".into(),
                "text".into(),
                "--to".into(),
                to.clone(),
                "--message".into(),
                message,
            ],
        )
        .await
    }

    async fn send_file(
        &self,
        to: String,
        path: String,
        caption: Option<String>,
        media_as: Option<String>,
        voice: bool,
    ) -> Response {
        let path = match std::fs::canonicalize(&path)
            .with_context(|| format!("attachment does not exist: {path}"))
        {
            Ok(path) if path.is_file() => path,
            Ok(path) => {
                return Response::failure(format!(
                    "attachment is not a regular file: {}",
                    path.display()
                ));
            }
            Err(error) => return Response::failure(format!("{error:#}")),
        };
        let action = if voice {
            "whatsapp.send_voice"
        } else {
            "whatsapp.send_file"
        };
        let mut args = if voice {
            vec!["send".into(), "voice".into()]
        } else {
            vec!["send".into(), "file".into()]
        };
        args.extend([
            "--to".into(),
            to.clone(),
            "--file".into(),
            path.to_string_lossy().into_owned(),
        ]);
        push_optional(&mut args, "--caption", caption);
        push_optional(&mut args, "--as", media_as);
        self.audited_send(action, &to, &args).await
    }

    async fn audited_send(&self, action: &str, target: &str, args: &[String]) -> Response {
        match self.wacli.run_json_with_warning(args).await {
            Ok((data, warning)) => {
                let status = if warning.is_some() {
                    "sent_with_warning"
                } else {
                    "sent"
                };
                let _ =
                    self.store
                        .record_action(action, target, status, &json!({"warning":warning}));
                Response {
                    ok: true,
                    data: Some(data),
                    error: None,
                    warning,
                }
            }
            Err(error) => {
                let rendered = format!("{error:#}");
                let _ =
                    self.store
                        .record_action(action, target, "failed", &json!({"error":rendered}));
                Response::failure(rendered)
            }
        }
    }

    async fn download_media(&self, message_id: String, chat: String) -> Result<Value> {
        let chat = self.wacli.resolve_download_chat(&message_id, &chat).await?;
        // Media download writes connector metadata and therefore needs wacli's
        // exclusive store lock. Serialize such operations and ask the
        // supervisor to stop its follow-sync child before invoking the command.
        let _operation = self.connector_operation.lock().await;
        let _pause = SyncPauseGuard::activate(self.sync_pause.clone())?;
        timeout(Duration::from_secs(10), async {
            while self.sync_running.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("timed out pausing WhatsApp sync for media download")?;
        self.wacli
            .run_json(&[
                "media".into(),
                "download".into(),
                "--chat".into(),
                chat,
                "--id".into(),
                message_id,
            ])
            .await
    }

    async fn handle_result(&self, request: Request) -> Result<Value> {
        match request {
            Request::Ping => Ok(json!({"version": crate::APP_VERSION})),
            Request::Status => Ok(
                json!({"daemon":"running","wacli_sync_process_running":self.sync_running.load(Ordering::Relaxed),"store":self.store.stats()?}),
            ),
            Request::Stats => self.store.stats(),
            Request::Doctor => {
                let driver = self.wacli.run_json(&["doctor".into()]).await;
                let sync_running = self.sync_running.load(Ordering::Relaxed);
                Ok(
                    json!({"orbit_database":"healthy","wacli_sync_process_running":sync_running,"wacli":match driver { Ok(v)=>decorate_doctor_status(v, sync_running), Err(e)=>json!({"healthy":false,"error":format!("{e:#}")}) }}),
                )
            }
            Request::Chats { unread_only, limit } => {
                let mut args = vec![
                    "chats".into(),
                    "list".into(),
                    "--limit".into(),
                    limit.to_string(),
                ];
                if unread_only {
                    args.push("--unread".into());
                }
                self.wacli
                    .run_json(&args)
                    .await
                    .map(|v| command_result("chats", &v))
            }
            Request::Contacts { query, limit } => {
                if query.trim().is_empty() {
                    self.wacli.list_contacts(limit).await
                } else {
                    self.wacli
                        .run_json(&[
                            "contacts".into(),
                            "search".into(),
                            query,
                            "--limit".into(),
                            limit.to_string(),
                        ])
                        .await
                }
            }
            Request::Messages { chat, limit } => {
                self.wacli
                    .run_json(&[
                        "messages".into(),
                        "list".into(),
                        "--chat".into(),
                        chat,
                        "--limit".into(),
                        limit.to_string(),
                    ])
                    .await
            }
            Request::Search {
                query,
                chat,
                from,
                after,
                before,
                limit,
            } => {
                // Prefer Orbit's normalized FTS for an unfiltered query. Filtered
                // searches delegate to wacli until equivalent indexes are added.
                if chat.is_none() && from.is_none() && after.is_none() && before.is_none() {
                    return self.store.search(&query, limit);
                }
                let mut args = vec![
                    "messages".into(),
                    "search".into(),
                    query,
                    "--limit".into(),
                    limit.to_string(),
                ];
                push_optional(&mut args, "--chat", chat);
                push_optional(&mut args, "--from", from);
                push_optional(&mut args, "--after", after);
                push_optional(&mut args, "--before", before);
                self.wacli.run_json(&args).await
            }
            Request::SendText { .. } | Request::SendFile { .. } => {
                unreachable!("send requests are handled by the audited mutation path")
            }
            Request::Download { message_id, chat } => self.download_media(message_id, chat).await,
            Request::Reconcile => {
                Ok(json!({"ingested":reconcile(&self.wacli,&self.store,1_000).await?}))
            }
            Request::Shutdown => {
                let _ = self.shutdown.send(true);
                Ok(json!({"shutdown":true}))
            }
        }
    }
}

/// A running daemon intentionally owns wacli's exclusive store lock. Reframe
/// that state so doctor output does not misdiagnose normal operation as a
/// disconnected external process while still avoiding an unprovable network
/// connectivity claim.
fn decorate_doctor_status(mut value: Value, sync_running: bool) -> Value {
    if sync_running
        && value.get("lock_held").and_then(Value::as_bool) == Some(true)
        && value.get("connection_state").and_then(Value::as_str) == Some("locked_by_other_process")
        && let Some(object) = value.as_object_mut()
    {
        object.insert("connected".into(), Value::Null);
        object.insert("connection_state".into(), json!("managed_by_daemon"));
        object.insert("daemon_owns_lock".into(), json!(true));
    }
    value
}

fn push_optional(args: &mut Vec<String>, flag: &str, value: Option<String>) {
    if let Some(value) = value {
        args.push(flag.into());
        args.push(value);
    }
}

pub async fn run(paths: OrbitPaths, config: Config) -> Result<()> {
    paths.create()?;
    let store = Store::new(paths.database.clone());
    store.initialize()?;
    let wacli = Wacli::new(config.resolved_wacli(&paths), paths.whatsapp_store.clone());
    wacli.ensure_compatible().await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (sync_pause_tx, sync_pause_rx) = watch::channel(false);
    let sync_running = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DaemonState {
        wacli: wacli.clone(),
        store: store.clone(),
        sync_running: sync_running.clone(),
        sync_pause: sync_pause_tx,
        connector_operation: Arc::new(Mutex::new(())),
        shutdown: shutdown_tx.clone(),
    });

    let mut secret_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut secret_bytes);
    let webhook_secret = hex::encode(secret_bytes);
    let webhook_url = format!("http://127.0.0.1:{}/v1/wacli", config.webhook_port);

    let mut webhook_task = tokio::spawn(webhook::serve(
        store.clone(),
        config.webhook_port,
        webhook_secret.clone(),
        shutdown_rx.clone(),
    ));
    let supervisor_task = tokio::spawn(supervise(
        wacli.clone(),
        config.clone(),
        webhook_url,
        webhook_secret,
        sync_running,
        sync_pause_rx,
        shutdown_rx.clone(),
    ));
    let reconcile_task = tokio::spawn(reconcile_loop(
        wacli,
        store,
        config.reconcile_limit,
        Duration::from_secs(config.reconcile_interval_seconds.max(5)),
        shutdown_rx.clone(),
    ));
    let handler: Handler = Arc::new(move |request| {
        let state = state.clone();
        Box::pin(async move { state.handle(request).await })
    });
    info!(endpoint=%paths.ipc_name(), "Orbit daemon ready");
    let ipc_name = paths.ipc_name();
    let mut ipc_task =
        tokio::spawn(async move { ipc::serve(&ipc_name, handler, shutdown_rx).await });
    // IPC shutdown is the normal exit. A webhook bind/server failure is fatal:
    // continuing would violate the low-latency ingestion guarantee.
    let result = tokio::select! {
        result = &mut ipc_task => result.context("IPC task panicked")?,
        result = &mut webhook_task => result.context("webhook task panicked")?,
    };
    let _ = shutdown_tx.send(true);
    ipc_task.abort();
    webhook_task.abort();
    supervisor_task.abort();
    reconcile_task.abort();
    result
}

async fn reconcile_loop(
    wacli: Wacli,
    store: Store,
    limit: u32,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if let Err(error) = reconcile(&wacli, &store, limit).await {
            warn!(%error, "message reconciliation failed");
        }
        tokio::select! {
            () = sleep(interval) => {}
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
        }
    }
}

async fn reconcile(wacli: &Wacli, store: &Store, limit: u32) -> Result<usize> {
    let limit = limit.max(1);
    let mut inserted = 0;
    // v3 also replays rows once to merge live LID chat identities into the
    // canonical phone JIDs used by wacli's durable database. Replays are
    // idempotent and repair installations created by older cursor algorithms.
    let mut row_cursor = store.sync_cursor("message_row_id_v3")?;
    let mut mutation_cursor = store.sync_cursor("message_mutation_ts")?;
    loop {
        let batch = wacli
            .reconcile_messages(row_cursor, mutation_cursor, limit)
            .await?;
        let page = batch
            .messages
            .iter()
            .map(|(raw, message)| {
                let kind = if message.revoked {
                    "message.revoked"
                } else if message.edited {
                    "message.edited"
                } else {
                    "message.created"
                };
                (kind, raw, message)
            })
            .collect::<Vec<_>>();
        inserted += store.ingest_batch(&page)?;
        // Cursors advance only after the entire batch is durable in Orbit. A
        // crash before this point merely replays idempotent payloads.
        store.set_sync_cursor("message_row_id_v3", batch.max_row_id)?;
        store.set_sync_cursor("message_mutation_ts", batch.max_mutation_timestamp)?;
        row_cursor = batch.max_row_id;
        mutation_cursor = batch.max_mutation_timestamp;
        if batch.new_rows_count < limit as usize {
            break;
        }
    }
    Ok(inserted)
}

async fn supervise(
    wacli: Wacli,
    config: Config,
    webhook_url: String,
    webhook_secret: String,
    running: Arc<AtomicBool>,
    mut pause: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = 1_u64;
    loop {
        // Exclusive connector commands (currently media download) temporarily
        // own the wacli store. Stay paused until their RAII guard releases it.
        while *pause.borrow() {
            tokio::select! {
                changed = pause.changed() => { if changed.is_err() { return; } }
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
            }
        }
        let mut child = match wacli.spawn_sync(
            &webhook_url,
            &webhook_secret,
            config.max_messages,
            &config.max_database_size,
        ) {
            Ok(child) => child,
            Err(error) => {
                warn!(%error, "could not start wacli sync");
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        running.store(true, Ordering::Relaxed);
        let stderr = child.stderr.take();
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    info!(target:"wacli", "{line}");
                }
            });
        }
        tokio::select! {
            status = child.wait() => { running.store(false,Ordering::Relaxed); warn!(?status,"wacli sync exited; restarting"); }
            changed = shutdown.changed() => { running.store(false,Ordering::Relaxed); let _=child.kill().await; if changed.is_err() || *shutdown.borrow() { return; } }
            changed = pause.changed() => {
                if changed.is_err() { let _=child.kill().await; running.store(false,Ordering::Relaxed); return; }
                if *pause.borrow() { let _=child.kill().await; running.store(false,Ordering::Relaxed); }
            }
        }
        if *pause.borrow() {
            continue;
        }
        tokio::select! {
            () = sleep(Duration::from_secs(backoff)) => {}
            changed = pause.changed() => { if changed.is_err() { return; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
        }
        backoff = (backoff * 2).min(60);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_identifies_the_daemon_owned_connector_lock() {
        let raw = json!({
            "connected": false,
            "connection_state": "locked_by_other_process",
            "lock_held": true
        });
        let decorated = decorate_doctor_status(raw, true);
        assert_eq!(decorated["connection_state"], "managed_by_daemon");
        assert_eq!(decorated["daemon_owns_lock"], true);
        assert!(decorated["connected"].is_null());
    }

    #[test]
    fn sync_pause_guard_resumes_on_drop() {
        let (sender, receiver) = watch::channel(false);
        {
            let _guard = SyncPauseGuard::activate(sender).unwrap();
            assert!(*receiver.borrow());
        }
        assert!(!*receiver.borrow());
    }
}
