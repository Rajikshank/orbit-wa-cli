//! Connector-neutral data contracts and local daemon protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NormalizedMessage {
    pub account_id: String,
    pub chat_external_id: String,
    pub chat_name: String,
    pub external_id: String,
    pub sender_external_id: String,
    pub sender_name: String,
    pub timestamp: String,
    pub from_me: bool,
    pub text: String,
    pub content_kind: String,
    pub media_caption: String,
    pub filename: String,
    pub mime_type: String,
    pub local_path: String,
    pub edited: bool,
    pub revoked: bool,
}

/// Commands are intentionally narrow. Raw arbitrary wacli execution is not
/// exposed because it would bypass Orbit's safety and audit boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Doctor,
    Stats,
    Chats {
        unread_only: bool,
        limit: u32,
    },
    Contacts {
        query: String,
        limit: u32,
    },
    Messages {
        chat: String,
        limit: u32,
    },
    Search {
        query: String,
        chat: Option<String>,
        from: Option<String>,
        after: Option<String>,
        before: Option<String>,
        limit: u32,
    },
    SendText {
        to: String,
        message: String,
    },
    SendFile {
        to: String,
        path: String,
        caption: Option<String>,
        media_as: Option<String>,
        voice: bool,
    },
    Download {
        message_id: String,
        chat: String,
    },
    Reconcile,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A warning is distinct from failure, especially for sends that reached
    /// WhatsApp but could not be recorded locally and must not be retried blindly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl Response {
    #[must_use]
    pub fn success(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            warning: None,
        }
    }

    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
            warning: None,
        }
    }
}
