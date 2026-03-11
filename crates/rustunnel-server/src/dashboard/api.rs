//! Dashboard REST API routes.
//!
//! All routes under `/api/` require a `Authorization: Bearer <token>` header
//! that is validated against the `tokens` table.  The single exception is
//! `GET /api/status` which returns a 200 OK without authentication.
//!
//! # Endpoints
//!
//! | Method | Path                                           | Description                        |
//! |--------|------------------------------------------------|------------------------------------|
//! | GET    | /api/status                                    | Server health                      |
//! | GET    | /api/tunnels                                   | All active tunnels                 |
//! | GET    | /api/tunnels/:id                               | Single tunnel info                 |
//! | GET    | /api/tunnels/:id/requests                      | Recent captured requests           |
//! | POST   | /api/tunnels/:id/replay/:request_id            | Replay a captured request          |
//! | GET    | /api/tokens                                    | List tokens (hash masked)          |
//! | POST   | /api/tokens                                    | Create a new token                 |
//! | DELETE | /api/tokens/:id                                | Delete a token                     |

use std::sync::Arc;

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

use crate::audit::{AuditEvent, AuditTx};
use crate::core::TunnelCore;
use crate::dashboard::capture::{load_requests_from_db, CaptureStore};
use crate::db;

// ── shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ApiState {
    pub core: Arc<TunnelCore>,
    pub pool: SqlitePool,
    pub capture: CaptureStore,
    pub admin_token: String,
    pub audit_tx: AuditTx,
}

// ── router ────────────────────────────────────────────────────────────────────

pub fn router(state: ApiState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // public
        .route("/api/status", get(status_handler))
        // authenticated
        .route("/api/tunnels", get(list_tunnels))
        .route("/api/tunnels/:id", get(get_tunnel))
        .route("/api/tunnels/:id", delete(force_close_tunnel))
        .route("/api/tunnels/:id/requests", get(tunnel_requests))
        .route("/api/tunnels/:id/replay/:request_id", post(replay_request))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/:id", delete(delete_token))
        .layer(cors)
        .with_state(state)
}

// ── auth helper ───────────────────────────────────────────────────────────────

/// Validate `Authorization: Bearer <token>` against the DB token table.
/// Also accepts the admin token directly.
async fn require_auth(
    headers: &HeaderMap,
    state: &ApiState,
) -> Result<(), (StatusCode, Json<ErrBody>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if auth.is_empty() {
        return Err(unauthorized("missing token"));
    }

    // Check admin token first (avoids DB hit for the most common case).
    if auth == state.admin_token {
        return Ok(());
    }

    match db::verify_token(&state.pool, auth).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(unauthorized("invalid token")),
        Err(e) => {
            warn!("token verification DB error: {e}");
            Err(unauthorized("invalid token"))
        }
    }
}

// ── response helpers ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrBody {
    error: String,
}

fn unauthorized(msg: &str) -> (StatusCode, Json<ErrBody>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrBody {
            error: msg.to_string(),
        }),
    )
}

fn not_found(msg: &str) -> (StatusCode, Json<ErrBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrBody {
            error: msg.to_string(),
        }),
    )
}

// ── handlers ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    active_sessions: usize,
    active_tunnels: usize,
}

async fn status_handler(State(state): State<ApiState>) -> impl IntoResponse {
    Json(StatusResponse {
        ok: true,
        active_sessions: state.core.sessions.len(),
        active_tunnels: state.core.http_routes.len() + state.core.tcp_routes.len(),
    })
}

// ── tunnels ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TunnelSummary {
    tunnel_id: String,
    protocol: String,
    label: String,
    public_url: String,
    /// ISO-8601 UTC timestamp when the tunnel was registered.
    connected_since: String,
    /// Total proxied requests / connections through this tunnel.
    request_count: u64,
    /// Remote address of the client that owns this tunnel.
    client_addr: String,
}

/// Convert an `Instant` recorded at tunnel creation into an ISO-8601 UTC string.
fn instant_to_iso(created: std::time::Instant) -> String {
    let elapsed = created.elapsed();
    let system_time = SystemTime::now()
        .checked_sub(elapsed)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let secs = system_time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format as RFC-3339 without pulling in chrono for this helper.
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .to_rfc3339();
    dt
}

async fn list_tunnels(headers: HeaderMap, State(state): State<ApiState>) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    let mut tunnels: Vec<TunnelSummary> = Vec::new();

    for entry in state.core.http_routes.iter() {
        let info = entry.value();
        let client_addr = state
            .core
            .sessions
            .get(&info.session_id)
            .map(|s| s.client_addr.to_string())
            .unwrap_or_default();
        tunnels.push(TunnelSummary {
            tunnel_id: info.tunnel_id.to_string(),
            protocol: "http".into(),
            label: entry.key().clone(),
            public_url: format!("https://{}", entry.key()),
            connected_since: instant_to_iso(info.created_at),
            request_count: info.request_count.load(Ordering::Relaxed),
            client_addr,
        });
    }

    for entry in state.core.tcp_routes.iter() {
        let info = entry.value();
        let client_addr = state
            .core
            .sessions
            .get(&info.session_id)
            .map(|s| s.client_addr.to_string())
            .unwrap_or_default();
        tunnels.push(TunnelSummary {
            tunnel_id: info.tunnel_id.to_string(),
            protocol: "tcp".into(),
            label: entry.key().to_string(),
            public_url: format!("tcp://:{}", entry.key()),
            connected_since: instant_to_iso(info.created_at),
            request_count: info.request_count.load(Ordering::Relaxed),
            client_addr,
        });
    }

    Json(tunnels).into_response()
}

async fn force_close_tunnel(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    let tunnel_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid tunnel id").into_response(),
    };

    state.core.remove_tunnel(&tunnel_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_tunnel(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    // Search HTTP routes first.
    for entry in state.core.http_routes.iter() {
        if entry.value().tunnel_id.to_string() == id {
            let info = entry.value();
            let client_addr = state
                .core
                .sessions
                .get(&info.session_id)
                .map(|s| s.client_addr.to_string())
                .unwrap_or_default();
            return Json(TunnelSummary {
                tunnel_id: info.tunnel_id.to_string(),
                protocol: "http".into(),
                label: entry.key().clone(),
                public_url: format!("https://{}", entry.key()),
                connected_since: instant_to_iso(info.created_at),
                request_count: info.request_count.load(Ordering::Relaxed),
                client_addr,
            })
            .into_response();
        }
    }

    // Then TCP routes.
    for entry in state.core.tcp_routes.iter() {
        if entry.value().tunnel_id.to_string() == id {
            let info = entry.value();
            let client_addr = state
                .core
                .sessions
                .get(&info.session_id)
                .map(|s| s.client_addr.to_string())
                .unwrap_or_default();
            return Json(TunnelSummary {
                tunnel_id: info.tunnel_id.to_string(),
                protocol: "tcp".into(),
                label: entry.key().to_string(),
                public_url: format!("tcp://:{}", entry.key()),
                connected_since: instant_to_iso(info.created_at),
                request_count: info.request_count.load(Ordering::Relaxed),
                client_addr,
            })
            .into_response();
        }
    }

    not_found("tunnel not found").into_response()
}

// ── captured requests ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RequestsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn tunnel_requests(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(tunnel_id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<RequestsQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    // Try in-memory ring buffer first for low-latency reads.
    {
        let guard = state.capture.read().await;
        if let Some(deque) = guard.get(&tunnel_id) {
            let items: Vec<_> = deque.iter().rev().take(q.limit as usize).collect();
            return Json(items).into_response();
        }
    }

    // Fall back to DB.
    match load_requests_from_db(&state.pool, &tunnel_id, q.limit).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            warn!("DB query failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrBody {
                    error: e.to_string(),
                }),
            )
                .into_response()
        }
    }
}

async fn replay_request(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path((tunnel_id, request_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    match crate::dashboard::capture::get_request(&state.pool, &request_id).await {
        Ok(Some(req)) if req.tunnel_id == tunnel_id => {
            // Return the stored request body as the replay payload.
            Json(req).into_response()
        }
        Ok(Some(_)) => not_found("request does not belong to this tunnel").into_response(),
        Ok(None) => not_found("request not found").into_response(),
        Err(e) => {
            warn!("replay DB query failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrBody {
                    error: e.to_string(),
                }),
            )
                .into_response()
        }
    }
}

// ── tokens ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTokenBody {
    label: String,
    /// Optional scope: comma-separated subdomain patterns.
    /// Omit or set to null for an unrestricted token.
    scope: Option<String>,
}

#[derive(Serialize)]
struct CreateTokenResponse {
    id: String,
    label: String,
    /// Raw token — shown only once at creation time.
    token: String,
}

async fn list_tokens(headers: HeaderMap, State(state): State<ApiState>) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    match db::list_tokens(&state.pool).await {
        Ok(tokens) => Json(tokens).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn create_token(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Json(body): Json<CreateTokenBody>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }
    let is_admin = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == state.admin_token)
        .unwrap_or(false);

    match db::create_token(&state.pool, &body.label, body.scope.as_deref()).await {
        Ok((token_record, raw)) => {
            let _ = state.audit_tx.try_send(AuditEvent::TokenCreated {
                token_id: token_record.id.clone(),
                label: token_record.label.clone(),
                admin: is_admin,
            });
            (
                StatusCode::CREATED,
                Json(CreateTokenResponse {
                    id: token_record.id,
                    label: token_record.label,
                    token: raw,
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn delete_token(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }
    let is_admin = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == state.admin_token)
        .unwrap_or(false);

    match db::delete_token(&state.pool, &id).await {
        Ok(true) => {
            let _ = state.audit_tx.try_send(AuditEvent::TokenDeleted {
                token_id: id,
                admin: is_admin,
            });
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => not_found("token not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}
