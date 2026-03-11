//! Async audit logger.
//!
//! All security-sensitive events (auth attempts, tunnel lifecycle, token
//! management, admin actions) are serialised to a JSON-lines file via a
//! background task so that the hot-path is never blocked on disk I/O.
//!
//! When `audit_log_path` is not configured a no-op sender is returned that
//! silently drops every event.

use std::path::Path;

use chrono::Utc;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

// ── event types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    AuthAttempt {
        peer: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_id: Option<String>,
    },
    TunnelRegistered {
        session_id: String,
        tunnel_id: String,
        protocol: String,
        label: String,
    },
    TunnelRemoved {
        tunnel_id: String,
        label: String,
    },
    TokenCreated {
        token_id: String,
        label: String,
        admin: bool,
    },
    TokenDeleted {
        token_id: String,
        admin: bool,
    },
    AdminAction {
        action: String,
        detail: String,
    },
}

// ── sender alias ─────────────────────────────────────────────────────────────

pub type AuditTx = mpsc::Sender<AuditEvent>;

// ── public constructor ────────────────────────────────────────────────────────

/// Spawn the background writer task and return its channel sender.
/// Events are written as JSON lines to `path`, one line per event.
pub fn start_audit_logger(path: &Path) -> AuditTx {
    let path = path.to_path_buf();
    let (tx, mut rx) = mpsc::channel::<AuditEvent>(512);

    tokio::spawn(async move {
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(path = %path.display(), "cannot open audit log: {e}");
                return;
            }
        };

        while let Some(event) = rx.recv().await {
            let ts = Utc::now().to_rfc3339();
            let line = match serde_json::to_string(&event) {
                Ok(s) => format!("{{\"ts\":\"{ts}\",{}}}\n", &s[1..s.len() - 1]),
                Err(e) => {
                    tracing::warn!("audit serialise error: {e}");
                    continue;
                }
            };
            if let Err(e) = file.write_all(line.as_bytes()).await {
                tracing::warn!("audit write error: {e}");
            }
        }
    });

    tx
}

/// Return a sender that silently discards all events (used when audit logging
/// is disabled in config).
pub fn noop_audit() -> AuditTx {
    let (tx, _rx) = mpsc::channel(1);
    tx
}
