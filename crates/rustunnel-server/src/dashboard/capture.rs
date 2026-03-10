//! In-memory capture ring buffer + DB persistence.
//!
//! The `CaptureService` task drains the `CaptureTx` channel, stores each event
//! in a bounded per-tunnel VecDeque (latest 500), and persists the row to SQLite
//! for durable storage and replay.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::db::models::CapturedRequest;
use crate::edge::capture::CaptureEvent;
use crate::error::Result;

/// Maximum requests kept in the ring buffer per tunnel.
const RING_CAPACITY: usize = 500;

/// Shared handle to the capture service's in-memory ring buffer.
pub type CaptureStore = std::sync::Arc<RwLock<HashMap<String, VecDeque<CapturedRequest>>>>;

/// Spawn the background capture consumer.  Returns a `CaptureStore` that the
/// dashboard API reads from.
pub fn start_capture_service(
    mut rx: mpsc::Receiver<CaptureEvent>,
    pool: SqlitePool,
) -> CaptureStore {
    let store: CaptureStore = std::sync::Arc::new(RwLock::new(HashMap::new()));
    let store_clone = store.clone();

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let row = event_to_row(&event);

            // Persist to SQLite.
            if let Err(e) = persist_row(&pool, &row).await {
                warn!(conn_id = %event.conn_id, "capture persist failed: {e}");
            }

            // Insert into ring buffer.
            let mut guard = store_clone.write().await;
            let deque = guard
                .entry(event.tunnel_id.to_string())
                .or_insert_with(|| VecDeque::with_capacity(RING_CAPACITY));
            if deque.len() >= RING_CAPACITY {
                deque.pop_front();
            }
            deque.push_back(row);

            debug!(tunnel_id = %event.tunnel_id, "capture event stored");
        }
    });

    store
}

/// Load persisted requests for a tunnel from SQLite (newest-first).
pub async fn load_requests_from_db(
    pool: &SqlitePool,
    tunnel_id: &str,
    limit: i64,
) -> Result<Vec<CapturedRequest>> {
    let rows: Vec<CapturedRequest> = sqlx::query_as(
        r#"
        SELECT id, tunnel_id, conn_id, method, path, status,
               request_bytes, response_bytes, duration_ms, captured_at,
               request_body, response_body
        FROM   captured_requests
        WHERE  tunnel_id = ?
        ORDER  BY captured_at DESC
        LIMIT  ?
        "#,
    )
    .bind(tunnel_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Fetch a single captured request for replay.
pub async fn get_request(pool: &SqlitePool, id: &str) -> Result<Option<CapturedRequest>> {
    let row: Option<CapturedRequest> = sqlx::query_as(
        r#"
        SELECT id, tunnel_id, conn_id, method, path, status,
               request_bytes, response_bytes, duration_ms, captured_at,
               request_body, response_body
        FROM   captured_requests
        WHERE  id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn event_to_row(e: &CaptureEvent) -> CapturedRequest {
    let captured_at: DateTime<Utc> = e
        .captured_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| DateTime::from_timestamp(d.as_secs() as i64, 0).unwrap_or_else(Utc::now))
        .unwrap_or_else(|_| Utc::now());

    CapturedRequest {
        id: Uuid::new_v4().to_string(),
        tunnel_id: e.tunnel_id.to_string(),
        conn_id: e.conn_id.to_string(),
        method: e.method.clone(),
        path: e.path.clone(),
        status: e.status as i64,
        request_bytes: e.request_bytes as i64,
        response_bytes: e.response_bytes as i64,
        duration_ms: e.duration_ms as i64,
        captured_at,
        request_body: None,
        response_body: None,
    }
}

async fn persist_row(pool: &SqlitePool, row: &CapturedRequest) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO captured_requests
            (id, tunnel_id, conn_id, method, path, status,
             request_bytes, response_bytes, duration_ms, captured_at,
             request_body, response_body)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&row.id)
    .bind(&row.tunnel_id)
    .bind(&row.conn_id)
    .bind(&row.method)
    .bind(&row.path)
    .bind(row.status)
    .bind(row.request_bytes)
    .bind(row.response_bytes)
    .bind(row.duration_ms)
    .bind(row.captured_at.to_rfc3339())
    .bind(&row.request_body)
    .bind(&row.response_body)
    .execute(pool)
    .await?;

    Ok(())
}
