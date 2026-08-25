//! Newline-delimited JSON over OS-local IPC.
//!
//! Windows uses a named pipe; Unix uses a domain socket. No TCP port is exposed
//! for ordinary CLI commands.

use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::model::{Request, Response};

pub type ResponseFuture = Pin<Box<dyn Future<Output = Response> + Send>>;
pub type Handler = Arc<dyn Fn(Request) -> ResponseFuture + Send + Sync>;

async fn handle_stream<S>(stream: S, handler: Handler) -> Result<()>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handler(request).await,
            Err(error) => Response::failure(format!("invalid request: {error}")),
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
        write.flush().await?;
    }
    Ok(())
}

async fn exchange<S>(stream: S, request: &Request) -> Result<Response>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    write.write_all(&encoded).await?;
    write.flush().await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    if line.is_empty() {
        anyhow::bail!("daemon closed the IPC connection without a response");
    }
    serde_json::from_str(&line).context("decode daemon response")
}

#[cfg(windows)]
pub async fn serve(
    name: &str,
    handler: Handler,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut first = true;
    loop {
        let mut options = ServerOptions::new();
        // The pipe is local IPC only; never accept SMB/network pipe clients.
        options.reject_remote_clients(true);
        if first {
            options.first_pipe_instance(true);
        }
        let server = options
            .create(name)
            .with_context(|| format!("create named pipe {name}"))?;
        first = false;
        tokio::select! {
            result = server.connect() => {
                result.context("accept named pipe client")?;
                let handler = handler.clone();
                tokio::spawn(async move { let _ = handle_stream(server, handler).await; });
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return Ok(()); }
            }
        }
    }
}

#[cfg(windows)]
pub async fn request(name: &str, request: &Request) -> Result<Response> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let client = ClientOptions::new()
        .open(name)
        .with_context(|| "Orbit daemon is not running; start it with `orbit daemon start`")?;
    exchange(client, request).await
}

#[cfg(unix)]
pub async fn serve(
    name: &str,
    handler: Handler,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    use tokio::net::UnixListener;
    let path = std::path::Path::new(name);
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("remove stale socket {name}"))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind Unix socket {name}"))?;
    // `~/.orbit` is private too, but explicitly securing the socket keeps the
    // IPC boundary correct even when a custom --home path has loose parents.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure Unix socket {name}"))?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let handler = handler.clone();
                tokio::spawn(async move { let _ = handle_stream(stream, handler).await; });
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { let _ = std::fs::remove_file(path); return Ok(()); }
            }
        }
    }
}

#[cfg(unix)]
pub async fn request(name: &str, request: &Request) -> Result<Response> {
    let stream = tokio::net::UnixStream::connect(name)
        .await
        .with_context(|| "Orbit daemon is not running; start it with `orbit daemon start`")?;
    exchange(stream, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn os_local_transport_round_trips_json_and_shuts_down() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::config::OrbitPaths::for_root(temp.path().to_path_buf());
        paths.create().unwrap();
        let name = paths.ipc_name();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handler: Handler = Arc::new(|request| {
            Box::pin(async move {
                match request {
                    Request::Ping => Response::success(json!({"transport":"local"})),
                    _ => Response::failure("unexpected command"),
                }
            })
        });
        let server_name = name.clone();
        let task = tokio::spawn(async move { serve(&server_name, handler, shutdown_rx).await });
        let response = loop {
            match request(&name, &Request::Ping).await {
                Ok(response) => break response,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        };
        assert!(response.ok);
        assert_eq!(response.data.unwrap()["transport"], "local");
        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
