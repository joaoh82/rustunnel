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
//! | GET    | /api/tunnels/:id/udp-sessions                  | UDP tunnel session info            |
//! | GET    | /api/tunnels/:id/p2p-peers                     | P2P tunnel peer info               |
//! | POST   | /api/tunnels/:id/replay/:request_id            | Replay a captured request          |
//! | GET    | /api/tokens                                    | List tokens (hash masked)          |
//! | POST   | /api/tokens                                    | Create a new token                 |
//! | DELETE | /api/tokens/:id                                | Delete a token                     |
//! | GET    | /api/history                                   | Paginated tunnel history           |

use std::net::SocketAddr;
use std::sync::Arc;

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, patch, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

use crate::audit::{AuditEvent, AuditTx};
use crate::auth::{client_ip, secret_eq};
use crate::config::RegionSection;
use crate::core::TunnelCore;
use crate::dashboard::capture::{load_requests_from_db, CaptureStore};
use crate::db::{self, Db};

// ── shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ApiState {
    pub core: Arc<TunnelCore>,
    pub db: Db,
    pub capture: CaptureStore,
    pub admin_token: String,
    pub audit_tx: AuditTx,
    pub region: RegionSection,
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
        .route("/api/regions", get(regions_handler))
        .route("/api/openapi.json", get(openapi_spec))
        // authenticated
        .route("/api/tunnels", get(list_tunnels))
        .route("/api/tunnels/:id", get(get_tunnel))
        .route("/api/tunnels/:id", delete(force_close_tunnel))
        .route("/api/tunnels/:id/requests", get(tunnel_requests))
        .route("/api/tunnels/:id/replay/:request_id", post(replay_request))
        .route("/api/tunnels/:id/health-events", get(tunnel_health_events))
        .route("/api/groups", get(list_groups))
        .route("/api/groups/:protocol/:label/events", get(group_events_sse))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/:id", delete(delete_token))
        .route("/api/history", get(tunnel_history))
        .route("/api/tunnels/:id/udp-sessions", get(tunnel_udp_sessions))
        .route("/api/tunnels/:id/p2p-peers", get(tunnel_p2p_peers))
        // admin-only
        .route("/api/admin/tokens/:id", patch(admin_patch_token))
        .route("/api/admin/users", get(admin_list_users))
        .route("/api/admin/users/:id", get(admin_get_user))
        .route(
            "/api/admin/users/:id",
            axum::routing::put(admin_update_user),
        )
        .route("/api/admin/plans", get(admin_list_plans))
        .route("/api/admin/usage/platform", get(admin_platform_usage))
        .route("/api/admin/usage/daily", get(admin_usage_daily))
        .route("/api/admin/usage/bandwidth", get(admin_usage_bandwidth))
        .route("/api/admin/usage/tokens", get(admin_usage_tokens))
        .route("/api/admin/usage/by-user", get(admin_usage_by_user))
        .route("/api/admin/usage/by-token", get(admin_usage_by_token))
        .route("/api/admin/usage/leaderboard", get(admin_usage_leaderboard))
        .route(
            "/api/admin/metrics/users-over-time",
            get(admin_users_over_time),
        )
        .route("/api/admin/users/:id/tunnels", get(admin_list_user_tunnels))
        .route("/api/admin/users/:id/tokens", get(admin_list_user_tokens))
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), auth_throttle))
        .with_state(state)
}

// ── failed-auth throttle ──────────────────────────────────────────────────────

/// Throttle failed bearer-token attempts per client IP.
///
/// Wraps every `/api` route. Requests without an `Authorization` header pass
/// straight through (public endpoints; a missing token is not a guess). For
/// the rest, a client over its failed-attempt budget gets `429` before any
/// handler runs — and therefore before the admin token is ever compared —
/// and every `401` response records one failure against that client.
///
/// The client address comes from [`ConnectInfo`], so the router must be served
/// with `into_make_service_with_connect_info` (see `dashboard::run_dashboard`);
/// without it the throttle is a no-op. Behind nginx the loopback peer's
/// `X-Real-IP` is honoured — see [`client_ip`].
async fn auth_throttle(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    if !req.headers().contains_key(AUTHORIZATION) {
        return next.run(req).await;
    }
    let Some(peer) = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
    else {
        return next.run(req).await;
    };
    let ip = client_ip(
        peer,
        req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()),
    );

    if state.core.auth_limiter.is_limited(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrBody {
                error: "too many failed auth attempts".to_string(),
            }),
        )
            .into_response();
    }

    let resp = next.run(req).await;
    if resp.status() == StatusCode::UNAUTHORIZED {
        state.core.auth_limiter.record_failure(ip);
    }
    resp
}

// ── auth helper ───────────────────────────────────────────────────────────────

/// Tenant scope resolved from the `Authorization` header. Determines which
/// tunnels/groups a dashboard caller is allowed to see (TUNNEL-1).
///
/// - `Admin` — operator-level access (the `auth.admin_token`). Sees every
///   tunnel and group across every tenant. This is the historical
///   behaviour and must stay unchanged.
/// - `User(user_id)` — a DB token whose `tokens.user_id` is set. The
///   caller can only see tunnels owned by sessions whose token belongs to
///   that same `user_id` (so a user can list all of *their* tunnels even
///   across multiple tokens / multiple clients).
/// - `Token(token_id)` — a legacy / admin-issued DB token with no
///   `user_id` (e.g. the bootstrap "agent" tokens that predate the
///   platform-api). Visibility is narrowed to that specific token: you
///   only see tunnels opened by sessions authenticated with the same DB
///   token row. Picked over admin-equivalent so adding the user dashboard
///   surface can't widen visibility for these legacy tokens by accident.
#[derive(Clone, Debug)]
enum AuthScope {
    Admin,
    User(uuid::Uuid),
    Token(String),
}

impl AuthScope {
    /// Whether this caller can see a tunnel/group member owned by the
    /// session described by `(session_user_id, session_db_token_id)`.
    /// Sessions with neither value (auth disabled, admin-token control
    /// plane, …) are visible only to `Admin` callers.
    fn can_see(
        &self,
        session_user_id: Option<uuid::Uuid>,
        session_db_token_id: Option<&str>,
    ) -> bool {
        match self {
            AuthScope::Admin => true,
            AuthScope::User(uid) => session_user_id == Some(*uid),
            AuthScope::Token(tid) => session_db_token_id == Some(tid.as_str()),
        }
    }
}

/// Validate `Authorization: Bearer <token>` against the DB token table.
/// Also accepts the admin token directly. Returns the resolved
/// [`AuthScope`] so handlers can scope their results to the caller's
/// tenant.
async fn require_auth(
    headers: &HeaderMap,
    state: &ApiState,
) -> Result<AuthScope, (StatusCode, Json<ErrBody>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if auth.is_empty() {
        return Err(unauthorized("missing token"));
    }

    // Check admin token first (avoids DB hit for the most common case).
    if secret_eq(auth, &state.admin_token) {
        return Ok(AuthScope::Admin);
    }

    match db::verify_token(&state.db.pg, auth).await {
        Ok(Some(t)) => match t.user_id {
            Some(uid) => Ok(AuthScope::User(uid)),
            None => Ok(AuthScope::Token(t.id)),
        },
        Ok(None) => Err(unauthorized("invalid token")),
        Err(e) => {
            warn!("token verification DB error: {e}");
            Err(unauthorized("invalid token"))
        }
    }
}

/// Look up the owning session of a `TunnelInfo` (or P2P publisher) and
/// answer whether `scope` is allowed to see the member.
///
/// Sessions can disappear under us (the session map is mutated lock-free
/// from other tasks) — when that happens we deny access except for the
/// admin scope. That matches the issue's "404 for tunnels the caller
/// can't see" rule and avoids leaking a stale tunnel after its session
/// closed.
fn scope_sees_session(
    core: &crate::core::TunnelCore,
    scope: &AuthScope,
    session_id: &uuid::Uuid,
) -> bool {
    if matches!(scope, AuthScope::Admin) {
        return true;
    }
    match core.sessions.get(session_id) {
        Some(s) => scope.can_see(s.user_id, s.db_token_id.as_deref()),
        None => false,
    }
}

/// Walk the routing tables to find which session owns `tunnel_id`.
/// Returns `None` if the tunnel doesn't exist. Used by handlers that
/// resolve a tunnel by ID and need to enforce scope before returning
/// data or mutating state.
fn find_tunnel_owning_session(
    core: &crate::core::TunnelCore,
    tunnel_id: &uuid::Uuid,
) -> Option<uuid::Uuid> {
    for entry in core.http_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.info.session_id);
        }
    }
    for entry in core.tcp_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.info.session_id);
        }
    }
    for entry in core.udp_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.info.session_id);
        }
    }
    for entry in core.p2p_tunnels.iter() {
        let publisher = entry.value();
        if publisher.tunnel_info.tunnel_id == *tunnel_id {
            return Some(publisher.tunnel_info.session_id);
        }
    }
    None
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
struct RegionInfo {
    id: String,
    name: String,
    location: String,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    region: RegionInfo,
    active_sessions: usize,
    active_tunnels: usize,
}

async fn status_handler(State(state): State<ApiState>) -> impl IntoResponse {
    Json(StatusResponse {
        ok: true,
        region: RegionInfo {
            id: state.region.id.clone(),
            name: state.region.name.clone(),
            location: state.region.location.clone(),
        },
        active_sessions: state.core.sessions.len(),
        // Count members, not groups — Phase 1 has 1 member per group so the
        // numbers match historical data; later phases may diverge.
        active_tunnels: state
            .core
            .http_routes
            .iter()
            .map(|g| g.members.len())
            .sum::<usize>()
            + state
                .core
                .tcp_routes
                .iter()
                .map(|g| g.members.len())
                .sum::<usize>(),
    })
}

// ── regions ───────────────────────────────────────────────────────────────────

/// `GET /api/regions` — list all active regions from the shared database.
///
/// No authentication required: the region list is used by the client for
/// auto-select before a token has been obtained.
async fn regions_handler(State(state): State<ApiState>) -> impl IntoResponse {
    match db::list_regions(&state.db.pg).await {
        Ok(regions) => Json(regions).into_response(),
        Err(e) => {
            warn!("failed to list regions: {e}");
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
    /// Total bytes proxied through this tunnel (TUNNEL-8 Phase 5).
    bytes_proxied: u64,
    /// Remote address of the client that owns this tunnel.
    client_addr: String,
    /// Region ID of the server hosting this tunnel (e.g. "eu", "us").
    region_id: String,
    /// NAT type reported by the client (P2P tunnels only).
    #[serde(skip_serializing_if = "Option::is_none")]
    nat_type: Option<String>,
    /// Public mapped addresses from STUN probing (P2P tunnels only).
    #[serde(skip_serializing_if = "Option::is_none")]
    mapped_addrs: Option<Vec<String>>,
    /// Group identity when this tunnel is part of a load-balancing pool
    /// (TUNNEL-8 Phase 5). `None` for solo tunnels — the dashboard shows
    /// those exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<GroupRef>,
    /// Current health bit. Always `true` for tunnels without a configured
    /// `health_check`. `false` here means the dispatch path is currently
    /// excluding this member.
    healthy: bool,
    /// Current consecutive-failure streak from client probes; resets on
    /// each `TunnelHealthy`. `0` for tunnels without a probe configured.
    consecutive_failures: u32,
    /// Cumulative `TunnelUnhealthy` frames received for this member.
    /// Same series as `rustunnel_group_health_failures_total` per-member.
    total_health_failures: u64,
}

/// Group identity attached to a single tunnel summary.
#[derive(Serialize, Clone)]
struct GroupRef {
    /// User-supplied display name (`group:` field on the client config).
    name: String,
    /// First 8 hex chars of the group's `key_hash`. Lets dashboards
    /// distinguish two pools with the same name + key without exposing
    /// the full hash. Empty for solo tunnels.
    key_hash_short: String,
    /// Total members in the group (including this one).
    member_count: usize,
    /// Members of the group that are currently healthy.
    healthy_count: usize,
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
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

async fn list_tunnels(headers: HeaderMap, State(state): State<ApiState>) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut tunnels: Vec<TunnelSummary> = Vec::new();

    // One row per member, filtered to the caller's tenant scope. Admin
    // sees everything; user-scoped tokens see only members owned by
    // sessions belonging to that user; legacy / no-user-id tokens see
    // only members opened by sessions authenticated with that exact
    // token. Solo tunnels (Phase 1 case) render exactly as before —
    // `group` is `None`, `healthy` is `true`, the failure counters are
    // 0. Group members get the full `GroupRef`, the actual health bit,
    // and per-member failure counters so the dashboard can show "this
    // backend is currently down on its 3rd straight probe failure".
    for entry in state.core.http_routes.iter() {
        let subdomain = entry.key().clone();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                continue;
            }
            tunnels.push(member_to_summary(
                &member,
                "http",
                subdomain.clone(),
                format!("https://{subdomain}"),
                &state,
                group_ref.clone(),
            ));
        }
    }

    for entry in state.core.tcp_routes.iter() {
        let port = *entry.key();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                continue;
            }
            tunnels.push(member_to_summary(
                &member,
                "tcp",
                port.to_string(),
                format!("tcp://:{port}"),
                &state,
                group_ref.clone(),
            ));
        }
    }

    for entry in state.core.udp_routes.iter() {
        let port = *entry.key();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                continue;
            }
            tunnels.push(member_to_summary(
                &member,
                "udp",
                port.to_string(),
                format!("udp://:{port}"),
                &state,
                group_ref.clone(),
            ));
        }
    }

    for entry in state.core.p2p_tunnels.iter() {
        let publisher = entry.value();
        let info = &publisher.tunnel_info;
        if !scope_sees_session(&state.core, &scope, &info.session_id) {
            continue;
        }
        let client_addr = state
            .core
            .sessions
            .get(&info.session_id)
            .map(|s| s.client_addr.to_string())
            .unwrap_or_default();
        tunnels.push(TunnelSummary {
            tunnel_id: info.tunnel_id.to_string(),
            protocol: "p2p".into(),
            label: publisher.name.clone(),
            public_url: format!("p2p://{}", publisher.name),
            connected_since: instant_to_iso(info.created_at),
            request_count: info.request_count.load(Ordering::Relaxed),
            bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
            client_addr,
            region_id: state.region.id.clone(),
            nat_type: publisher.nat_type.clone(),
            mapped_addrs: if publisher.mapped_addrs.is_empty() {
                None
            } else {
                Some(publisher.mapped_addrs.clone())
            },
            // P2P tunnels don't participate in load-balancing groups.
            group: None,
            healthy: true,
            consecutive_failures: 0,
            total_health_failures: 0,
        });
    }

    Json(tunnels).into_response()
}

// ── helpers shared between /api/tunnels and /api/groups ──────────────────────

/// Build a `GroupRef` for a routing entry. Returns `None` for solo
/// (ungrouped) routes; their `TunnelSummary.group` stays `None` and the
/// dashboard renders them exactly as before TUNNEL-8.
fn group_summary_ref(group: &Arc<crate::core::TunnelGroup>) -> Option<GroupRef> {
    let key_hash = group.key_hash.as_deref()?;
    let healthy_count = group
        .members
        .iter()
        .filter(|m| m.healthy.load(Ordering::Acquire))
        .count();
    Some(GroupRef {
        name: group.name.clone(),
        key_hash_short: key_hash.chars().take(8).collect(),
        member_count: group.members.len(),
        healthy_count,
    })
}

/// Build a `TunnelSummary` for a single group member. Used by both
/// `/api/tunnels` and `/api/groups`.
fn member_to_summary(
    member: &crate::core::GroupMember,
    protocol: &str,
    label: String,
    public_url: String,
    state: &ApiState,
    group: Option<GroupRef>,
) -> TunnelSummary {
    let info = &member.info;
    let client_addr = state
        .core
        .sessions
        .get(&info.session_id)
        .map(|s| s.client_addr.to_string())
        .unwrap_or_default();
    TunnelSummary {
        tunnel_id: info.tunnel_id.to_string(),
        protocol: protocol.to_string(),
        label,
        public_url,
        connected_since: instant_to_iso(info.created_at),
        request_count: info.request_count.load(Ordering::Relaxed),
        bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
        client_addr,
        region_id: state.region.id.clone(),
        nat_type: None,
        mapped_addrs: None,
        group,
        healthy: member.healthy.load(Ordering::Acquire),
        consecutive_failures: member.consecutive_failures.load(Ordering::Acquire),
        total_health_failures: member.total_health_failures.load(Ordering::Relaxed),
    }
}

async fn force_close_tunnel(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let tunnel_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid tunnel id").into_response(),
    };

    // Admin short-circuit: preserves the historical idempotent
    // semantics (204 even when the tunnel doesn't exist) and avoids
    // walking every route to find an owner that doesn't matter.
    if matches!(scope, AuthScope::Admin) {
        state.core.remove_tunnel(&tunnel_id);
        return StatusCode::NO_CONTENT.into_response();
    }

    // Non-admin: resolve the tunnel's owning session before mutating
    // state so a user-scoped caller can't close another tenant's
    // tunnel. Return 404 (not 403) when the caller can't see the
    // tunnel — same shape as "tunnel doesn't exist" so we don't leak
    // existence.
    let owning_session = find_tunnel_owning_session(&state.core, &tunnel_id);
    let visible = match owning_session {
        Some(sid) => scope_sees_session(&state.core, &scope, &sid),
        None => false,
    };
    if !visible {
        return not_found("tunnel not found").into_response();
    }

    state.core.remove_tunnel(&tunnel_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_tunnel(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Search HTTP routes first.
    for entry in state.core.http_routes.iter() {
        let subdomain = entry.key().clone();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if member.info.tunnel_id.to_string() == id {
                if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                    return not_found("tunnel not found").into_response();
                }
                return Json(member_to_summary(
                    &member,
                    "http",
                    subdomain.clone(),
                    format!("https://{subdomain}"),
                    &state,
                    group_ref.clone(),
                ))
                .into_response();
            }
        }
    }

    // Then TCP routes.
    for entry in state.core.tcp_routes.iter() {
        let port = *entry.key();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if member.info.tunnel_id.to_string() == id {
                if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                    return not_found("tunnel not found").into_response();
                }
                return Json(member_to_summary(
                    &member,
                    "tcp",
                    port.to_string(),
                    format!("tcp://:{port}"),
                    &state,
                    group_ref.clone(),
                ))
                .into_response();
            }
        }
    }

    // Then UDP routes.
    for entry in state.core.udp_routes.iter() {
        let port = *entry.key();
        let group_arc = entry.value();
        let group_ref = group_summary_ref(group_arc);
        for member in group_arc.members.iter() {
            if member.info.tunnel_id.to_string() == id {
                if !scope_sees_session(&state.core, &scope, &member.info.session_id) {
                    return not_found("tunnel not found").into_response();
                }
                return Json(member_to_summary(
                    &member,
                    "udp",
                    port.to_string(),
                    format!("udp://:{port}"),
                    &state,
                    group_ref.clone(),
                ))
                .into_response();
            }
        }
    }

    // Then P2P tunnels.
    for entry in state.core.p2p_tunnels.iter() {
        let publisher = entry.value();
        if publisher.tunnel_info.tunnel_id.to_string() == id {
            let info = &publisher.tunnel_info;
            if !scope_sees_session(&state.core, &scope, &info.session_id) {
                return not_found("tunnel not found").into_response();
            }
            let client_addr = state
                .core
                .sessions
                .get(&info.session_id)
                .map(|s| s.client_addr.to_string())
                .unwrap_or_default();
            return Json(TunnelSummary {
                tunnel_id: info.tunnel_id.to_string(),
                protocol: "p2p".into(),
                label: publisher.name.clone(),
                public_url: format!("p2p://{}", publisher.name),
                connected_since: instant_to_iso(info.created_at),
                request_count: info.request_count.load(Ordering::Relaxed),
                bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
                client_addr,
                region_id: state.region.id.clone(),
                nat_type: publisher.nat_type.clone(),
                mapped_addrs: if publisher.mapped_addrs.is_empty() {
                    None
                } else {
                    Some(publisher.mapped_addrs.clone())
                },
                group: None,
                healthy: true,
                consecutive_failures: 0,
                total_health_failures: 0,
            })
            .into_response();
        }
    }

    not_found("tunnel not found").into_response()
}

// ── health-event timeline (TUNNEL-8 Phase 5) ────────────────────────────────

#[derive(Serialize)]
struct HealthEventRow {
    /// RFC-3339 UTC timestamp of the transition.
    at: String,
    healthy: bool,
    reason: String,
}

/// `GET /api/tunnels/:id/health-events` — recent health-state transitions
/// for a single tunnel (whichever protocol it lives under). Returns
/// oldest → newest, capped at `HEALTH_EVENT_RING_SIZE` (50). 404 when the
/// tunnel doesn't exist or is a P2P tunnel (P2P doesn't participate in
/// load balancing and has no probe state).
async fn tunnel_health_events(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let tunnel_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid tunnel id").into_response(),
    };

    // 404 (not 403) when the caller can't see this tunnel — same shape
    // as "tunnel doesn't exist" so existence isn't leaked.
    let owning_session = find_tunnel_owning_session(&state.core, &tunnel_id);
    if !owning_session
        .map(|sid| scope_sees_session(&state.core, &scope, &sid))
        .unwrap_or(false)
    {
        return not_found("tunnel not found").into_response();
    }

    // Walk the routing tables to find this tunnel's GroupMember and
    // snapshot its ring. Same shape as `get_tunnel`.
    let snapshot = find_member_health_events(&state.core, &tunnel_id);
    match snapshot {
        Some(events) => {
            let rows: Vec<HealthEventRow> = events
                .into_iter()
                .map(|e| HealthEventRow {
                    at: chrono::DateTime::<chrono::Utc>::from(e.at).to_rfc3339(),
                    healthy: e.healthy,
                    reason: e.reason,
                })
                .collect();
            Json(rows).into_response()
        }
        None => not_found("tunnel not found").into_response(),
    }
}

/// Resolve `tunnel_id` to its `GroupMember` and return a snapshot of the
/// member's health-event ring. None if the tunnel is unknown or a P2P
/// publisher (which doesn't have a `GroupMember`).
fn find_member_health_events(
    core: &crate::core::TunnelCore,
    tunnel_id: &uuid::Uuid,
) -> Option<Vec<crate::core::HealthEvent>> {
    for entry in core.http_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.health_events_snapshot());
        }
    }
    for entry in core.tcp_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.health_events_snapshot());
        }
    }
    for entry in core.udp_routes.iter() {
        if let Some(member) = entry.value().members.get(tunnel_id) {
            return Some(member.health_events_snapshot());
        }
    }
    None
}

// ── load-balancing groups (TUNNEL-8 Phase 5) ────────────────────────────────

/// One member's contribution inside a `GroupSummary` row. Subset of
/// `TunnelSummary` — just the fields needed for the dashboard's
/// per-member breakdown of a group.
#[derive(Serialize)]
struct GroupMemberSummary {
    tunnel_id: String,
    session_id: String,
    client_addr: String,
    request_count: u64,
    bytes_proxied: u64,
    healthy: bool,
    consecutive_failures: u32,
    total_health_failures: u64,
    /// ISO-8601 UTC timestamp the member registered.
    connected_since: String,
    /// Probe type configured for this member (`tcp` / `http`), or `None`
    /// when the member didn't opt into health checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    health_check_kind: Option<String>,
    /// Whether this member registered with a per-tenant
    /// `health_check.alert_webhook` URL. We expose presence only — the
    /// URL itself stays server-side so a dashboard viewer can't exfil
    /// another tenant's notification destination. The 🔔 indicator on
    /// the member row reads from this.
    has_alert_webhook: bool,
}

/// One row per active multi-member group across HTTP and TCP.
///
/// Solo (ungrouped) tunnels are NOT returned by this endpoint — they
/// already show up in `/api/tunnels` and don't have a group identity to
/// surface. To list everything, the dashboard joins this endpoint with
/// `/api/tunnels` on the client side.
#[derive(Serialize)]
struct GroupSummary {
    /// `http` / `tcp`. UDP and P2P don't support groups today.
    protocol: String,
    /// Routing key — the subdomain for HTTP, port-as-string for TCP.
    label: String,
    /// User-supplied display name from `RegisterTunnel.group`.
    name: String,
    /// First 8 hex chars of the SHA-256 key hash. Stable identity across
    /// reconnects so a single group can be tracked across churn.
    key_hash_short: String,
    region_id: String,
    member_count: usize,
    healthy_count: usize,
    unhealthy_count: usize,
    /// Sum of `request_count` across the group's members.
    total_dispatches: u64,
    /// Sum of `total_health_failures` across the group's members.
    total_health_failures: u64,
    members: Vec<GroupMemberSummary>,
}

async fn list_groups(headers: HeaderMap, State(state): State<ApiState>) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut groups: Vec<GroupSummary> = Vec::new();
    let region = state.region.id.clone();

    // Per-group filtering rules:
    // - Admin sees every group with full aggregates.
    // - Other scopes only see groups where at least one member's owning
    //   session is visible to them, and every counter
    //   (`member_count`, `healthy_count`, `unhealthy_count`,
    //   `total_dispatches`, `total_health_failures`) reflects only the
    //   visible subset. Today's group-key model is single-tenant in
    //   practice, but filtering aggregates as well keeps the response
    //   safe under any future multi-tenant pool — no cross-tenant
    //   counts ever leak through.
    for entry in state.core.http_routes.iter() {
        let group = entry.value();
        if group.key_hash.is_none() {
            continue; // solo registration — covered by /api/tunnels
        }
        if let Some(summary) = group_to_summary(
            "http",
            entry.key().clone(),
            group,
            &state.core,
            &region,
            &scope,
        ) {
            groups.push(summary);
        }
    }

    for entry in state.core.tcp_routes.iter() {
        let group = entry.value();
        if group.key_hash.is_none() {
            continue;
        }
        if let Some(summary) = group_to_summary(
            "tcp",
            entry.key().to_string(),
            group,
            &state.core,
            &region,
            &scope,
        ) {
            groups.push(summary);
        }
    }

    Json(groups).into_response()
}

/// Build a `GroupSummary` for a single routing entry, filtered to the
/// caller's `scope`. Returns `None` when the caller can't see any
/// member of this group — the row is dropped from the response.
fn group_to_summary(
    protocol: &str,
    label: String,
    group: &Arc<crate::core::TunnelGroup>,
    core: &Arc<crate::core::TunnelCore>,
    region_id: &str,
    scope: &AuthScope,
) -> Option<GroupSummary> {
    let key_hash = group.key_hash.as_deref().unwrap_or("");
    let mut healthy_count = 0;
    let mut unhealthy_count = 0;
    let mut total_dispatches: u64 = 0;
    let mut total_health_failures: u64 = 0;
    let mut visible_member_count = 0usize;
    let mut members = Vec::with_capacity(group.members.len());
    for m in group.members.iter() {
        let info = &m.info;
        if !scope_sees_session(core, scope, &info.session_id) {
            continue;
        }
        let healthy = m.healthy.load(Ordering::Acquire);
        if healthy {
            healthy_count += 1;
        } else {
            unhealthy_count += 1;
        }
        let req_count = info.request_count.load(Ordering::Relaxed);
        let failures = m.total_health_failures.load(Ordering::Relaxed);
        total_dispatches += req_count;
        total_health_failures += failures;
        visible_member_count += 1;
        let client_addr = core
            .sessions
            .get(&info.session_id)
            .map(|s| s.client_addr.to_string())
            .unwrap_or_default();
        let health_check_kind = m.health_spec.as_ref().map(|s| match s.kind {
            rustunnel_protocol::HealthCheckKind::Tcp => "tcp".to_string(),
            rustunnel_protocol::HealthCheckKind::Http => "http".to_string(),
        });
        let has_alert_webhook = m
            .health_spec
            .as_ref()
            .and_then(|s| s.alert_webhook_url.as_deref())
            .is_some();
        members.push(GroupMemberSummary {
            tunnel_id: info.tunnel_id.to_string(),
            session_id: info.session_id.to_string(),
            client_addr,
            request_count: req_count,
            bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
            healthy,
            consecutive_failures: m.consecutive_failures.load(Ordering::Acquire),
            total_health_failures: failures,
            connected_since: instant_to_iso(info.created_at),
            health_check_kind,
            has_alert_webhook,
        });
    }
    if members.is_empty() {
        return None;
    }
    // Aggregates reflect only visible members. For admin callers this
    // matches the historical full-pool numbers because every member is
    // visible. For tenant callers it never includes members from
    // sessions they aren't allowed to see.
    Some(GroupSummary {
        protocol: protocol.to_string(),
        label,
        name: group.name.clone(),
        key_hash_short: key_hash.chars().take(8).collect(),
        region_id: region_id.to_string(),
        member_count: visible_member_count,
        healthy_count,
        unhealthy_count,
        total_dispatches,
        total_health_failures,
        members,
    })
}

// ── /api/groups/:protocol/:label/events — SSE (TUNNEL-8 Phase 5) ────────────

/// Live event stream for a single load-balancing group.
///
/// Subscribes to the global `group_events` broadcast channel and filters
/// to events matching `(protocol, label)`. Each event is serialised as
/// JSON and emitted as a default-named SSE event. A 30s keep-alive ping
/// keeps proxies from idle-closing the connection. The stream survives
/// across member churn — when the group has no members the stream stays
/// open but goes silent until the group is recreated.
///
/// On a `Lagged(n)` error from the broadcast channel (subscriber fell
/// behind because the producer outran the consumer), the stream emits a
/// `lagged` event with the dropped count and continues. Clients that
/// see a `lagged` event should resync via `GET /api/groups`.
async fn group_events_sse(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path((protocol, label)): Path<(String, String)>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Resolve the group up-front and verify the caller can see it. We
    // look it up by `(protocol, label)` in the routing tables and check
    // that at least one member is visible to the caller. 404 otherwise
    // — same shape as a non-existent group, no existence leak. After
    // the initial check the per-event filter only emits events for
    // members the caller is allowed to see, so member churn (a new
    // foreign-tenant member joins a previously visible group) can't
    // exfil events from outside the caller's scope.
    let group_visible = match protocol.as_str() {
        "http" => state
            .core
            .http_routes
            .get(&label)
            .map(|g| {
                g.members
                    .iter()
                    .any(|m| scope_sees_session(&state.core, &scope, &m.info.session_id))
            })
            .unwrap_or(false),
        "tcp" => label
            .parse::<u16>()
            .ok()
            .and_then(|p| state.core.tcp_routes.get(&p))
            .map(|g| {
                g.members
                    .iter()
                    .any(|m| scope_sees_session(&state.core, &scope, &m.info.session_id))
            })
            .unwrap_or(false),
        _ => false,
    };
    if !matches!(scope, AuthScope::Admin) && !group_visible {
        return not_found("group not found").into_response();
    }

    let receiver = state.core.subscribe_group_events();
    let stream = tokio_stream::wrappers::BroadcastStream::new(receiver);
    let proto = protocol;
    let lbl = label;
    let core = Arc::clone(&state.core);
    let stream_scope = scope.clone();
    let filtered = futures_util::StreamExt::filter_map(stream, move |result| {
        let proto = proto.clone();
        let lbl = lbl.clone();
        let core = Arc::clone(&core);
        let stream_scope = stream_scope.clone();
        async move {
            match result {
                Ok(event) => {
                    if event.protocol == proto && event.label == lbl {
                        // Drop events for members the caller can't see
                        // — protects against a foreign tenant joining
                        // the same group later in the stream.
                        if !matches!(stream_scope, AuthScope::Admin) {
                            let owning = find_tunnel_owning_session(&core, &event.tunnel_id);
                            let visible = owning
                                .map(|sid| scope_sees_session(&core, &stream_scope, &sid))
                                .unwrap_or(false);
                            if !visible {
                                return None;
                            }
                        }
                        let payload = serde_json::to_string(&event).ok()?;
                        Some(Ok::<_, std::convert::Infallible>(
                            axum::response::sse::Event::default()
                                .event("group_event")
                                .data(payload),
                        ))
                    } else {
                        None
                    }
                }
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(axum::response::sse::Event::default()
                        .event("lagged")
                        .data(n.to_string())))
                }
            }
        }
    });

    axum::response::sse::Sse::new(filtered)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(std::time::Duration::from_secs(30))
                .text("keep-alive"),
        )
        .into_response()
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
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Captured requests are tied to a tunnel — same scope check as the
    // `/api/tunnels/:id` endpoint. 404 (not 403) when the caller can't
    // see the tunnel so we don't leak existence. We require the tunnel
    // to be live in-memory (the dashboard typically captures only for
    // live tunnels); a tunnel that has gone away can't be looked up by
    // owner so we deny non-admin callers in that case.
    let parsed = tunnel_id.parse::<uuid::Uuid>().ok();
    let visible = match parsed {
        Some(tid) => find_tunnel_owning_session(&state.core, &tid)
            .map(|sid| scope_sees_session(&state.core, &scope, &sid))
            .unwrap_or(matches!(scope, AuthScope::Admin)),
        None => matches!(scope, AuthScope::Admin),
    };
    if !visible {
        return not_found("tunnel not found").into_response();
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
    match load_requests_from_db(&state.db.local, &tunnel_id, q.limit).await {
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
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Same scope check as `/api/tunnels/:id/requests`: 404 if the caller
    // can't see the parent tunnel. Stops a non-admin tenant from
    // replaying captured requests they didn't generate.
    let parsed = tunnel_id.parse::<uuid::Uuid>().ok();
    let visible = match parsed {
        Some(tid) => find_tunnel_owning_session(&state.core, &tid)
            .map(|sid| scope_sees_session(&state.core, &scope, &sid))
            .unwrap_or(matches!(scope, AuthScope::Admin)),
        None => matches!(scope, AuthScope::Admin),
    };
    if !visible {
        return not_found("tunnel not found").into_response();
    }

    match crate::dashboard::capture::get_request(&state.db.local, &request_id).await {
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

    match db::list_tokens_with_counts(&state.db.pg).await {
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
        .map(|t| secret_eq(t, &state.admin_token))
        .unwrap_or(false);

    match db::create_token(&state.db.pg, &body.label, body.scope.as_deref()).await {
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
        .map(|t| secret_eq(t, &state.admin_token))
        .unwrap_or(false);

    match db::delete_token(&state.db.pg, &id).await {
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

// ── admin routes ──────────────────────────────────────────────────────────────

/// Require the request to carry the admin token (not a DB token).
async fn require_admin(
    headers: &HeaderMap,
    state: &ApiState,
) -> Result<(), (StatusCode, Json<ErrBody>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if secret_eq(auth, &state.admin_token) {
        Ok(())
    } else {
        Err(unauthorized("admin token required"))
    }
}

#[derive(Deserialize)]
struct PatchTokenBody {
    unlimited: bool,
}

/// `PATCH /api/admin/tokens/:id` — toggle the `unlimited` flag on a token.
///
/// Requires the admin token. Takes effect on the next tunnel registration
/// attempt (the per-session token cache is not invalidated).
async fn admin_patch_token(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PatchTokenBody>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }

    match db::set_token_unlimited(&state.db.pg, &id, body.unlimited).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
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

// ── admin user routes ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AdminUsersQuery {
    #[serde(default = "default_admin_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    search: Option<String>,
    /// Effective plan, e.g. "free" | "payg". Churned paid users match "free".
    plan: Option<String>,
    /// "google" | "password".
    auth_method: Option<String>,
    /// User account status, e.g. "active" | "banned".
    status: Option<String>,
    /// Only users who were on a paid plan and have since canceled.
    #[serde(default)]
    previously_paid: bool,
    /// "created" (default) | "last_active" | "email". Unknown values fall back
    /// to the default rather than erroring.
    sort: Option<String>,
}

fn default_admin_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct AdminUsersResponse {
    users: Vec<db::AdminUser>,
    total: i64,
}

/// `GET /api/admin/users` — paginated user list.
async fn admin_list_users(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<AdminUsersQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }

    let filters = db::AdminUserFilters {
        search: q.search.as_deref(),
        plan: q.plan.as_deref(),
        auth_method: q.auth_method.as_deref(),
        status: q.status.as_deref(),
        previously_paid: q.previously_paid,
    };
    let sort = q
        .sort
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let (users, total) = tokio::join!(
        db::list_admin_users(&state.db.pg, q.limit, q.offset, filters, sort),
        db::count_admin_users(&state.db.pg, filters),
    );
    match (users, total) {
        (Ok(users), Ok(total)) => Json(AdminUsersResponse { users, total }).into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// `GET /api/admin/users/:id` — single user detail.
async fn admin_get_user(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }

    let user_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid user id").into_response(),
    };

    match db::get_admin_user(&state.db.pg, &user_id).await {
        Ok(Some(user)) => Json(user).into_response(),
        Ok(None) => not_found("user not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateUserBody {
    /// New status: "active" | "banned" | "suspended"
    status: String,
}

/// `PUT /api/admin/users/:id` — update user status (ban, unban, suspend).
async fn admin_update_user(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }

    // Validate status value.
    let allowed = ["active", "banned", "suspended"];
    if !allowed.contains(&body.status.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrBody {
                error: format!("status must be one of: {}", allowed.join(", ")),
            }),
        )
            .into_response();
    }

    let user_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid user id").into_response(),
    };

    match db::set_user_status(&state.db.pg, &user_id, &body.status).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("user not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// ── admin plan / usage / per-user routes ──────────────────────────────────────

/// `GET /api/admin/plans` — list all plans with their active subscriber counts.
async fn admin_list_plans(headers: HeaderMap, State(state): State<ApiState>) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    match db::list_admin_plans(&state.db.pg).await {
        Ok(plans) => Json(plans).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// `GET /api/admin/usage/platform` — platform-wide aggregate ops metrics.
///
/// Queries the shared PostgreSQL so the result covers all regions.
async fn admin_platform_usage(
    headers: HeaderMap,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    // Count live tunnels from in-memory state (matches the Tunnels tab source
    // of truth). Members, not groups — keeps the historical metric meaning.
    let live_tunnels = (state
        .core
        .http_routes
        .iter()
        .map(|g| g.members.len())
        .sum::<usize>()
        + state
            .core
            .tcp_routes
            .iter()
            .map(|g| g.members.len())
            .sum::<usize>()) as i64;

    match db::get_platform_usage(&state.db.pg).await {
        Ok(mut usage) => {
            usage.active_tunnels_global = live_tunnels;
            Json(usage).into_response()
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

// ── admin metrics routes ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UsersOverTimeQuery {
    /// Number of trailing days to include (1–365, default 30).
    #[serde(default = "default_uot_days")]
    days: i64,
}

fn default_uot_days() -> i64 {
    30
}

/// `GET /api/admin/metrics/users-over-time?days=30`
///
/// Returns `days` daily data points (oldest → newest) with user-registration
/// counts, filling zeroes for days with no registrations.
/// Requires the admin token.
async fn admin_users_over_time(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsersOverTimeQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 365);
    match db::get_users_over_time(&state.db.pg, days).await {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct UserTunnelsQuery {
    #[serde(default = "default_admin_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

#[derive(Serialize)]
struct UserTunnelsResponse {
    entries: Vec<db::AdminTunnelEntry>,
    total: i64,
}

/// `GET /api/admin/users/:id/tunnels` — paginated tunnel history for one user.
async fn admin_list_user_tunnels(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<UserTunnelsQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let user_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid user id").into_response(),
    };
    let (entries, total) = tokio::join!(
        db::list_user_tunnels(&state.db.pg, &user_id, q.limit, q.offset),
        db::count_user_tunnels(&state.db.pg, &user_id),
    );
    match (entries, total) {
        (Ok(entries), Ok(total)) => Json(UserTunnelsResponse { entries, total }).into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// `GET /api/admin/users/:id/tokens` — all tokens belonging to one user.
async fn admin_list_user_tokens(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let user_id = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return not_found("invalid user id").into_response(),
    };
    match db::list_user_tokens(&state.db.pg, &user_id).await {
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

// ── admin usage rollup routes ─────────────────────────────────────────────────
//
// These read `usage_daily` (and `tokens` for token counts). They do not
// scan `tunnel_log`. `exclude_test` defaults to true and drops users whose
// email matches `joaoh82+%` or `joao.silva@carv.com`, case-insensitively.
// System tokens (`tokens.system`) are always left out.
//
// Daily bytes count a tunnel on the UTC day it closes. When the window
// includes today, the byte-bearing responses carry `note` so a reader
// can see that an open tunnel is missing from today's bytes.

/// Shown when a usage window includes today (UTC). Open tunnels have no
/// byte row until they close, so today's bandwidth is a partial day.
const CLOSED_TUNNEL_BYTE_NOTE: &str = "Today's bytes include only closed tunnels.";

fn usage_byte_note(from: chrono::NaiveDate, to: chrono::NaiveDate) -> Option<&'static str> {
    let today = chrono::Utc::now().date_naive();
    if from <= today && today <= to {
        Some(CLOSED_TUNNEL_BYTE_NOTE)
    } else {
        None
    }
}

#[derive(Deserialize)]
struct UsageWindowQuery {
    /// Trailing UTC days, 1 through 90. Default 30.
    #[serde(default = "default_usage_days")]
    days: i64,
    /// Drop test-account emails. Default true. Pass `false` to include them.
    #[serde(default = "default_exclude_test")]
    exclude_test: bool,
}

#[derive(Deserialize)]
struct UsagePageQuery {
    #[serde(default = "default_usage_days")]
    days: i64,
    #[serde(default = "default_exclude_test")]
    exclude_test: bool,
    #[serde(default = "default_usage_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

#[derive(Deserialize)]
struct LeaderboardQuery {
    /// 7, 30, or 90. Default 30.
    #[serde(default = "default_usage_days")]
    days: i64,
    /// `bandwidth`, `tunnel_hours`, or `peak_concurrent`. Default `bandwidth`.
    #[serde(default = "default_leaderboard_sort")]
    sort: String,
    #[serde(default = "default_exclude_test")]
    exclude_test: bool,
    #[serde(default = "default_leaderboard_limit")]
    limit: i64,
}

fn default_usage_days() -> i64 {
    30
}

fn default_exclude_test() -> bool {
    true
}

fn default_usage_limit() -> i64 {
    100
}

fn default_leaderboard_sort() -> String {
    "bandwidth".to_string()
}

fn default_leaderboard_limit() -> i64 {
    100
}

fn bad_request(msg: &str) -> (StatusCode, Json<ErrBody>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrBody {
            error: msg.to_string(),
        }),
    )
}

fn usage_range(days: i64) -> (chrono::NaiveDate, chrono::NaiveDate) {
    let days = days.clamp(1, 90);
    let end = chrono::Utc::now().date_naive();
    let start = end
        .checked_sub_days(chrono::Days::new((days as u64).saturating_sub(1)))
        .unwrap_or(end);
    (start, end)
}

fn db_err(e: crate::error::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrBody {
            error: e.to_string(),
        }),
    )
        .into_response()
}

/// `GET /api/admin/usage/daily?days=30&exclude_test=true`
///
/// One point per UTC day: sessions, tunnel-seconds, tunnel-hours, active
/// users, and requests. Days with no rows are zeroes.
async fn admin_usage_daily(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsageWindowQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 90);
    let (from, to) = usage_range(days);
    match db::usage_daily_series(&state.db.pg, from, to, q.exclude_test).await {
        Ok(series) => Json(serde_json::json!({
            "days": days,
            "exclude_test": q.exclude_test,
            "note": usage_byte_note(from, to),
            "series": series,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

/// `GET /api/admin/usage/bandwidth?days=30&exclude_test=true`
///
/// Daily bytes. `bytes_in` and `bytes_out` are null when the whole day is
/// legacy `bytes_proxied` with no direction. `bytes_total` is always the
/// combined total. `bytes_direction_unknown` is the legacy part of that total.
async fn admin_usage_bandwidth(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsageWindowQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 90);
    let (from, to) = usage_range(days);
    match db::usage_bandwidth_series(&state.db.pg, from, to, q.exclude_test).await {
        Ok(series) => Json(serde_json::json!({
            "days": days,
            "exclude_test": q.exclude_test,
            "note": usage_byte_note(from, to),
            "series": series,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

/// `GET /api/admin/usage/tokens?days=30&exclude_test=true`
///
/// `total` is every non-system token. `active_last_7_days` counts tokens
/// whose `last_used_at` is within 7 days (a verify, not a tunnel open).
/// `created_per_day` covers the `days` window. `days` does not change the
/// 7-day active count.
async fn admin_usage_tokens(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsageWindowQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 90);
    let (from, to) = usage_range(days);
    match db::token_usage_stats(&state.db.pg, from, to, q.exclude_test).await {
        Ok(stats) => Json(serde_json::json!({
            "days": days,
            "exclude_test": q.exclude_test,
            "total": stats.total,
            "active_last_7_days": stats.active_last_7_days,
            "created_per_day": stats.created_per_day,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

/// `GET /api/admin/usage/by-user?days=30&exclude_test=true&limit=100&offset=0`
///
/// Sessions, tunnel-hours, bandwidth, requests, and peak concurrent tunnels
/// per user. Peak is the max daily user-wide overlap in the window.
async fn admin_usage_by_user(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsagePageQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 90);
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let (from, to) = usage_range(days);
    match db::usage_by_user(&state.db.pg, from, to, q.exclude_test, limit, offset).await {
        Ok((entries, total)) => Json(serde_json::json!({
            "days": days,
            "exclude_test": q.exclude_test,
            "note": usage_byte_note(from, to),
            "total": total,
            "entries": entries,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

/// `GET /api/admin/usage/by-token?days=30&exclude_test=true&limit=100&offset=0`
///
/// Same measures per token. Peak is the max daily overlap for that token
/// across regions and protocols. System tokens are omitted.
async fn admin_usage_by_token(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<UsagePageQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    let days = q.days.clamp(1, 90);
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let (from, to) = usage_range(days);
    match db::usage_by_token(&state.db.pg, from, to, q.exclude_test, limit, offset).await {
        Ok((entries, total)) => Json(serde_json::json!({
            "days": days,
            "exclude_test": q.exclude_test,
            "note": usage_byte_note(from, to),
            "total": total,
            "entries": entries,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

/// `GET /api/admin/usage/leaderboard?days=7|30|90&sort=bandwidth|tunnel_hours|peak_concurrent`
///
/// Top users. `days` must be 7, 30, or 90. Bandwidth sorts by total bytes
/// (legacy rows included). Peak sorts by the max daily concurrent tunnels.
async fn admin_usage_leaderboard(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<LeaderboardQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_admin(&headers, &state).await {
        return e.into_response();
    }
    if !matches!(q.days, 7 | 30 | 90) {
        return bad_request("days must be 7, 30, or 90").into_response();
    }
    let Some(sort) = db::LeaderboardSort::parse(&q.sort) else {
        return bad_request("sort must be bandwidth, tunnel_hours, or peak_concurrent")
            .into_response();
    };
    let limit = q.limit.clamp(1, 100);
    let (from, to) = usage_range(q.days);
    match db::usage_leaderboard(&state.db.pg, from, to, q.exclude_test, sort, limit).await {
        Ok(entries) => Json(serde_json::json!({
            "days": q.days,
            "sort": q.sort,
            "exclude_test": q.exclude_test,
            "note": usage_byte_note(from, to),
            "entries": entries,
        }))
        .into_response(),
        Err(e) => db_err(e),
    }
}

// ── OpenAPI spec ──────────────────────────────────────────────────────────────

/// `GET /api/openapi.json` — machine-readable description of the REST API.
///
/// Returned without authentication so that AI agents and developer tooling can
/// discover available endpoints before obtaining a token.
async fn openapi_spec() -> impl IntoResponse {
    Json(serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "rustunnel REST API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "REST API for managing tunnels, tokens, and viewing tunnel history."
        },
        "servers": [
            { "url": "/", "description": "This server" }
        ],
        "paths": {
            "/api/status": {
                "get": {
                    "summary": "Server health check",
                    "operationId": "getStatus",
                    "security": [],
                    "responses": {
                        "200": {
                            "description": "Server is healthy",
                            "content": { "application/json": { "schema": {
                                "type": "object",
                                "properties": {
                                    "ok":              { "type": "boolean" },
                                    "active_sessions": { "type": "integer" },
                                    "active_tunnels":  { "type": "integer" }
                                }
                            }}}
                        }
                    }
                }
            },
            "/api/tunnels": {
                "get": {
                    "summary": "List all active tunnels",
                    "operationId": "listTunnels",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "Array of tunnel objects" },
                        "401": { "description": "Unauthorized" }
                    }
                }
            },
            "/api/tunnels/{id}": {
                "get": {
                    "summary": "Get a single tunnel by UUID",
                    "operationId": "getTunnel",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": { "description": "Tunnel object" },
                        "404": { "description": "Not found" }
                    }
                },
                "delete": {
                    "summary": "Force-close an active tunnel",
                    "operationId": "closeTunnel",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "204": { "description": "Tunnel removed" },
                        "404": { "description": "Not found" }
                    }
                }
            },
            "/api/tunnels/{id}/requests": {
                "get": {
                    "summary": "List recent captured HTTP requests for a tunnel",
                    "operationId": "tunnelRequests",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "id",    "in": "path",  "required": true,  "schema": { "type": "string" } },
                        { "name": "limit", "in": "query", "required": false, "schema": { "type": "integer", "default": 50 } }
                    ],
                    "responses": { "200": { "description": "Array of captured request objects" } }
                }
            },
            "/api/tokens": {
                "get": {
                    "summary": "List all API tokens",
                    "operationId": "listTokens",
                    "security": [{ "bearerAuth": [] }],
                    "responses": { "200": { "description": "Array of token objects" } }
                },
                "post": {
                    "summary": "Create a new API token",
                    "operationId": "createToken",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string" },
                                "scope": { "type": "string", "nullable": true }
                            },
                            "required": ["label"]
                        }}}
                    },
                    "responses": {
                        "201": { "description": "Token created — raw value shown once" },
                        "401": { "description": "Unauthorized" }
                    }
                }
            },
            "/api/tokens/{id}": {
                "delete": {
                    "summary": "Delete an API token",
                    "operationId": "deleteToken",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "204": { "description": "Token deleted" },
                        "404": { "description": "Not found" }
                    }
                }
            },
            "/api/history": {
                "get": {
                    "summary": "Paginated tunnel registration history",
                    "operationId": "getTunnelHistory",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "limit",    "in": "query", "schema": { "type": "integer", "default": 50 } },
                        { "name": "offset",   "in": "query", "schema": { "type": "integer", "default": 0 } },
                        { "name": "protocol", "in": "query", "schema": { "type": "string", "enum": ["http","tcp"] } }
                    ],
                    "responses": { "200": { "description": "{ total, entries[] }" } }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Admin token or API token created via POST /api/tokens"
                }
            }
        }
    }))
}

// ── tunnel history ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_history_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// Filter by protocol: "http" or "tcp".
    protocol: Option<String>,
    /// Filter by API token ID.
    token_id: Option<String>,
    /// Filter by status: true = active (open), false = closed.
    active: Option<bool>,
    /// Sort column: "started" (default), "duration", or "protocol".
    #[serde(default = "default_sort_by")]
    sort_by: String,
    /// Sort direction: "desc" (default) or "asc".
    #[serde(default = "default_sort_dir")]
    sort_dir: String,
}

fn default_history_limit() -> i64 {
    50
}

fn default_sort_by() -> String {
    "started".to_string()
}

fn default_sort_dir() -> String {
    "desc".to_string()
}

#[derive(Serialize)]
struct TunnelHistoryResponse {
    entries: Vec<crate::db::models::TunnelLogEntry>,
    total: i64,
}

async fn tunnel_history(
    headers: HeaderMap,
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&headers, &state).await {
        return e.into_response();
    }

    let proto = q.protocol.as_deref();
    let token_id = q.token_id.as_deref();

    let (entries, total) = tokio::join!(
        db::list_tunnel_history(
            &state.db.pg,
            q.limit,
            q.offset,
            proto,
            token_id,
            q.active,
            &q.sort_by,
            &q.sort_dir,
        ),
        db::count_tunnel_history(&state.db.pg, proto, token_id, q.active),
    );

    match (entries, total) {
        (Ok(entries), Ok(total)) => Json(TunnelHistoryResponse { entries, total }).into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrBody {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// ── UDP sessions ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct UdpSessionInfo {
    tunnel_id: String,
    port: u16,
    protocol: String,
    request_count: u64,
    bytes_proxied: u64,
    connected_since: String,
}

async fn tunnel_udp_sessions(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Find the UDP tunnel by ID.
    for entry in state.core.udp_routes.iter() {
        let port = *entry.key();
        for member in entry.value().members.iter() {
            let info = &member.info;
            if info.tunnel_id.to_string() == id {
                if !scope_sees_session(&state.core, &scope, &info.session_id) {
                    return not_found("UDP tunnel not found").into_response();
                }
                return Json(UdpSessionInfo {
                    tunnel_id: info.tunnel_id.to_string(),
                    port,
                    protocol: "udp".into(),
                    request_count: info.request_count.load(Ordering::Relaxed),
                    bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
                    connected_since: instant_to_iso(info.created_at),
                })
                .into_response();
            }
        }
    }

    not_found("UDP tunnel not found").into_response()
}

// ── P2P peers ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct P2pPeerInfo {
    tunnel_id: String,
    tunnel_name: String,
    publisher_session_id: String,
    request_count: u64,
    bytes_proxied: u64,
    connected_since: String,
    nat_type: Option<String>,
    mapped_addrs: Option<Vec<String>>,
}

async fn tunnel_p2p_peers(
    headers: HeaderMap,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let scope = match require_auth(&headers, &state).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Find the P2P tunnel by ID.
    for entry in state.core.p2p_tunnels.iter() {
        let publisher = entry.value();
        let info = &publisher.tunnel_info;
        if info.tunnel_id.to_string() == id {
            if !scope_sees_session(&state.core, &scope, &info.session_id) {
                return not_found("P2P tunnel not found").into_response();
            }
            return Json(P2pPeerInfo {
                tunnel_id: info.tunnel_id.to_string(),
                tunnel_name: publisher.name.clone(),
                publisher_session_id: info.session_id.to_string(),
                request_count: info.request_count.load(Ordering::Relaxed),
                bytes_proxied: info.bytes_proxied.load(Ordering::Relaxed),
                nat_type: publisher.nat_type.clone(),
                mapped_addrs: if publisher.mapped_addrs.is_empty() {
                    None
                } else {
                    Some(publisher.mapped_addrs.clone())
                },
                connected_since: instant_to_iso(info.created_at),
            })
            .into_response();
        }
    }

    not_found("P2P tunnel not found").into_response()
}
