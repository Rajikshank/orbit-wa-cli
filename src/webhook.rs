//! Loopback-only signed webhook receiver for low-latency ingestion.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::watch;
use tracing::warn;

use crate::{store::Store, wacli::normalize_webhook_message};

#[derive(Clone)]
struct WebhookState {
    store: Store,
    secret: Arc<String>,
}

pub async fn serve(
    store: Store,
    port: u16,
    secret: String,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let state = WebhookState {
        store,
        secret: Arc::new(secret),
    };
    let app = Router::new()
        .route("/v1/wacli", post(receive))
        .with_state(state);
    // Binding IPv4 loopback only is a security invariant; this endpoint accepts
    // personal message content and must never be reachable from the LAN.
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("bind webhook receiver at {address}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
        })
        .await
        .context("serve webhook receiver")?;
    Ok(())
}

async fn receive(State(state): State<WebhookState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let signature = headers
        .get("X-Wacli-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !valid_signature(state.secret.as_bytes(), &body, signature) {
        return StatusCode::UNAUTHORIZED;
    }
    let raw = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            warn!(%error, "rejected invalid webhook JSON");
            return StatusCode::BAD_REQUEST;
        }
    };
    let message = match normalize_webhook_message(&raw) {
        Ok(message) => message,
        Err(error) => {
            warn!(%error, "rejected incompatible webhook payload");
            return StatusCode::UNPROCESSABLE_ENTITY;
        }
    };
    let kind = if message.revoked {
        "message.revoked"
    } else if message.edited {
        "message.edited"
    } else {
        "message.created"
    };
    match state.store.ingest(kind, &raw, &message) {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(error) => {
            warn!(%error, "failed to persist webhook");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn valid_signature(secret: &[u8], body: &[u8], provided: &str) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    let Some(encoded) = provided.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Ok(bytes) = hex::decode(encoded) else {
        return false;
    };
    mac.verify_slice(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_check_rejects_tampering() {
        let secret = b"secret";
        let body = br#"{"ID":"m1"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(valid_signature(secret, body, &signature));
        assert!(!valid_signature(secret, b"changed", &signature));
        assert!(!valid_signature(
            secret,
            body,
            signature.trim_start_matches("sha256=")
        ));
    }

    #[tokio::test]
    async fn signed_http_delivery_is_persisted_and_duplicate_safe() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("orbit.db"));
        store.initialize().unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let secret = "integration-secret";
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(serve(store.clone(), port, secret.into(), shutdown_rx));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let body = serde_json::to_vec(&serde_json::json!({
            "Chat":"123@s.whatsapp.net","ChatName":"Alex","ID":"http-m1",
            "SenderJID":"123@s.whatsapp.net","Timestamp":"2026-08-25T10:00:00Z",
            "FromMe":false,"Text":"signed delivery"
        }))
        .unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/wacli");
        let unauthorized = client.post(&url).body(body.clone()).send().await.unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        for _ in 0..2 {
            let accepted = client
                .post(&url)
                .header("X-Wacli-Signature", &signature)
                .body(body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
        }
        let stats = store.stats().unwrap();
        assert_eq!(stats["messages"], 1);
        assert_eq!(stats["source_events"], 1);
        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
