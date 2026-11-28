//! Plan limits, account access, and the unclaimed-agent browser warning.
//!
//! Limits come from the caller's active `plans` row (`max_tunnels`,
//! `allow_custom_subdomains`, `allowed_protocols`, `bandwidth_hard_cap`,
//! `bandwidth_limit_bytes`). Nothing here special-cases the plan name
//! except the browser warning, which must disappear when that row is no
//! longer [`AgentPlanLimits::NAME`]. Hobby (`billing_model = "free"`) keeps
//! its own upgrade copy in [`crate::billing`]. PAYG stays unlimited because
//! its row says so (`max_tunnels` NULL, every protocol, hard cap off).
//!
//! # Cache
//!
//! [`AccountGate`] caches the account snapshot for [`ACCOUNT_CACHE_TTL`] and
//! the month-byte total for [`BANDWIDTH_CACHE_TTL`]. That total is the
//! committed `tunnel_log` sum plus live counters this process has not
//! flushed. Bytes proxied after the snapshot are added on the next read, so
//! a tunnel that crosses the cap is refused on the following request,
//! inside the cache window. The column itself is written on the meter flush
//! (default 60s) and when the tunnel closes. Registration and token auth read
//! Postgres directly and fail closed. A background pass every
//! [`SESSION_RECHECK_INTERVAL`] re-reads tokens and users in one query and
//! closes sessions that are no longer allowed, including one whose committed
//! month bytes plus this process's unflushed counters have reached a hard
//! cap. An open stream therefore stops on the next pass even when the meter
//! flush is disabled. A database error on that pass or on a proxied request
//! serves the last cached account and byte total, including one past its
//! TTL. The request is proxied, and the session stays up, only when this
//! process has no cached entry at all.
//!
//! # Deactivation sweep
//!
//! [`deactivate_expired_unclaimed_agents`] is one `UPDATE`. Every edge may
//! run it. It never deletes a user or a token.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use rustunnel_protocol::TunnelProtocol;

use crate::billing::month_bytes_including_live;
use crate::core::{ControlMessage, TunnelCore};
use crate::db::models::{AgentPlanLimits, Token};
use crate::error::Result;

/// Per-request account snapshot. The session recheck still reads Postgres.
/// When that read fails it uses this snapshot, including after the TTL.
pub const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(30);
/// How long a committed month-byte total is reused. Bytes the live tunnel
/// proxies after that snapshot are still counted on the next request.
pub const BANDWIDTH_CACHE_TTL: Duration = Duration::from_secs(15);
/// How often live sessions are checked for a ban, suspension, deactivation,
/// an expired token, or a hard bandwidth cap.
pub const SESSION_RECHECK_INTERVAL: Duration = Duration::from_secs(20);
/// Bound on the session recheck's token and account query, and on the
/// month-byte read the hard cap uses. A stuck database returns in this time
/// instead of holding the recheck loop.
const STATEMENT_TIMEOUT: &str = "2s";
/// How often expired unclaimed agent accounts are marked `deactivated_at`.
pub const DEACTIVATION_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Host-only cookie set by Continue. The browser stores it for the tunnel
/// host, so a visitor is not warned again on that host for this long.
pub const WARNING_COOKIE_NAME: &str = "rustunnel_warn_ok";
pub const WARNING_COOKIE_MAX_AGE_SECS: u64 = 24 * 60 * 60;
/// Query flag the Continue link uses. The edge answers it itself and does
/// not forward it to the local service.
pub const WARNING_ACK_PARAM: &str = "rustunnel_warn_continue";
/// `rustunnel-skip-warning: 1` proxies the request with no interstitial.
pub const WARNING_SKIP_HEADER: &str = "rustunnel-skip-warning";

pub mod code {
    pub const PROTOCOL_NOT_ALLOWED: &str = "protocol_not_allowed";
    pub const CUSTOM_SUBDOMAIN_NOT_ALLOWED: &str = "custom_subdomain_not_allowed";
    pub const TUNNEL_LIMIT: &str = "tunnel_limit";
    pub const BANDWIDTH_CAP_REACHED: &str = "bandwidth_cap_reached";
    pub const TOKEN_EXPIRED: &str = "token_expired";
    pub const ACCOUNT_DEACTIVATED: &str = "account_deactivated";
    pub const ACCOUNT_SUSPENDED: &str = "account_suspended";
    pub const TOKEN_INACTIVE: &str = "token_inactive";
}

/// Wire name stored in `plans.allowed_protocols`.
pub fn protocol_name(protocol: &TunnelProtocol) -> &'static str {
    match protocol {
        TunnelProtocol::Http => "http",
        TunnelProtocol::Https => "https",
        TunnelProtocol::Tcp => "tcp",
        TunnelProtocol::Udp => "udp",
        TunnelProtocol::P2p => "p2p",
    }
}

/// Active plan columns the edge enforces. `name` is display and the warning
/// key, not a stand-in for the numeric limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLimits {
    pub name: String,
    pub billing_model: String,
    pub max_tunnels: Option<i32>,
    pub allow_custom_subdomains: bool,
    pub allowed_protocols: Vec<String>,
    pub bandwidth_hard_cap: bool,
    pub bandwidth_limit_bytes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    pub status: String,
    pub kind: String,
    pub claimed_at: Option<DateTime<Utc>>,
    pub deactivated_at: Option<DateTime<Utc>>,
    pub plan: Option<PlanLimits>,
}

/// Why a token must not open or keep a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBlock {
    TokenInactive,
    TokenExpired,
    Deactivated,
    Suspended { banned: bool },
}

impl AuthBlock {
    pub fn code(self) -> &'static str {
        match self {
            AuthBlock::TokenInactive => code::TOKEN_INACTIVE,
            AuthBlock::TokenExpired => code::TOKEN_EXPIRED,
            AuthBlock::Deactivated => code::ACCOUNT_DEACTIVATED,
            AuthBlock::Suspended { .. } => code::ACCOUNT_SUSPENDED,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            AuthBlock::TokenInactive => "token is suspended or revoked",
            AuthBlock::TokenExpired => "api token expired",
            AuthBlock::Deactivated => "account deactivated",
            AuthBlock::Suspended { banned: true } => "account banned",
            AuthBlock::Suspended { banned: false } => "account suspended",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationDenial {
    pub code: &'static str,
    pub message: String,
}

/// What the HTTP edge should do before it opens a tunnel stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeAction {
    Proxy,
    Respond(EdgeResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub set_cookie: Option<String>,
    pub location: Option<String>,
}

/// Headers the edge already pulled off the public request.
pub struct IncomingRequest<'a> {
    pub method: &'a str,
    pub accept: Option<&'a str>,
    pub sec_fetch_mode: Option<&'a str>,
    pub skip_warning: Option<&'a str>,
    pub cookie: Option<&'a str>,
    pub host: &'a str,
    pub path_and_query: &'a str,
    /// Public scheme. The continue cookie is `Secure` only on HTTPS.
    pub https: bool,
}

struct CachedView {
    view: AccountView,
    at: Instant,
}

struct CachedBytes {
    /// Committed rows plus unflushed live bytes at `at`.
    bytes: i64,
    at: Instant,
    /// In-memory totals for this user's tunnels when `bytes` was computed.
    live: Vec<(Uuid, u64)>,
}

/// Short-lived cache in front of account and bandwidth reads.
pub struct AccountGate {
    pool: PgPool,
    account_ttl: Duration,
    bandwidth_ttl: Duration,
    accounts: DashMap<Uuid, CachedView>,
    bandwidth: DashMap<Uuid, CachedBytes>,
}

impl AccountGate {
    pub fn new(pool: PgPool, account_ttl: Duration, bandwidth_ttl: Duration) -> Self {
        Self {
            pool,
            account_ttl,
            bandwidth_ttl,
            accounts: DashMap::new(),
            bandwidth: DashMap::new(),
        }
    }

    /// Account row plus the active plan. `None` when the user does not exist.
    ///
    /// A database error returns the last cached snapshot, including one past
    /// [`ACCOUNT_CACHE_TTL`]. The error is returned only when this user has
    /// never been cached.
    pub async fn view(&self, user_id: Uuid) -> Result<Option<AccountView>> {
        if let Some(hit) = self.fresh_view(user_id) {
            return Ok(Some(hit));
        }
        match load_account(&self.pool, user_id).await {
            Ok(loaded) => {
                if !self.account_ttl.is_zero() {
                    if let Some(ref view) = loaded {
                        self.accounts.insert(
                            user_id,
                            CachedView {
                                view: view.clone(),
                                at: Instant::now(),
                            },
                        );
                    } else {
                        self.accounts.remove(&user_id);
                    }
                }
                Ok(loaded)
            }
            Err(e) => match self.any_view(user_id) {
                Some(view) => {
                    tracing::warn!(
                        error = %e,
                        %user_id,
                        "account lookup failed; using cached account"
                    );
                    Ok(Some(view))
                }
                None => Err(e),
            },
        }
    }

    /// UTC-month bytes for `user_id`, including this process's unflushed counters.
    ///
    /// A fresh cache entry keeps the total from when it was stored and adds
    /// whatever those tunnels have proxied since. Crossing the cap therefore
    /// shows up on the next check, without waiting for the meter flush or
    /// for the cache to expire. A database error returns that total, even
    /// past [`BANDWIDTH_CACHE_TTL`], plus bytes proxied since the snapshot.
    /// The error is returned only when this user has never been cached.
    pub async fn month_bytes(&self, user_id: Uuid, core: &TunnelCore) -> Result<i64> {
        if let Some(hit) = self.cached_month_bytes(user_id, core) {
            return Ok(hit);
        }
        let live = core.live_bytes_proxied_for_user(user_id);
        match month_bytes_including_live(&self.pool, user_id, &live).await {
            Ok(bytes) => {
                if !self.bandwidth_ttl.is_zero() {
                    self.bandwidth.insert(
                        user_id,
                        CachedBytes {
                            bytes,
                            at: Instant::now(),
                            live,
                        },
                    );
                }
                Ok(bytes)
            }
            Err(e) => match self.any_month_bytes(user_id, core) {
                Some(bytes) => {
                    tracing::warn!(
                        error = %e,
                        %user_id,
                        "bandwidth lookup failed; using cached bytes"
                    );
                    Ok(bytes)
                }
                None => Err(e),
            },
        }
    }

    fn cached_month_bytes(&self, user_id: Uuid, core: &TunnelCore) -> Option<i64> {
        let cached = self.bandwidth.get(&user_id).and_then(|entry| {
            if fresh(entry.at, self.bandwidth_ttl) {
                Some((entry.bytes, entry.live.clone()))
            } else {
                None
            }
        })?;
        let (base, snapshot) = cached;
        let now = core.live_bytes_proxied_for_user(user_id);
        Some(base.saturating_add(live_growth(&snapshot, &now)))
    }

    fn fresh_view(&self, user_id: Uuid) -> Option<AccountView> {
        let entry = self.accounts.get(&user_id)?;
        if fresh(entry.at, self.account_ttl) {
            Some(entry.view.clone())
        } else {
            None
        }
    }

    fn any_view(&self, user_id: Uuid) -> Option<AccountView> {
        self.accounts.get(&user_id).map(|entry| entry.view.clone())
    }

    fn any_month_bytes(&self, user_id: Uuid, core: &TunnelCore) -> Option<i64> {
        let cached = self
            .bandwidth
            .get(&user_id)
            .map(|entry| (entry.bytes, entry.live.clone()))?;
        let (base, snapshot) = cached;
        let now = core.live_bytes_proxied_for_user(user_id);
        Some(base.saturating_add(live_growth(&snapshot, &now)))
    }
}

fn fresh(at: Instant, ttl: Duration) -> bool {
    !ttl.is_zero() && at.elapsed() < ttl
}

/// Bytes proxied since `snapshot`. Tunnels that were not open then count
/// in full. Tunnels that have since closed stay in the cached base.
fn live_growth(snapshot: &[(Uuid, u64)], now: &[(Uuid, u64)]) -> i64 {
    let base: std::collections::HashMap<Uuid, u64> = snapshot.iter().copied().collect();
    let mut extra = 0i64;
    for (id, bytes) in now {
        let previous = base.get(id).copied().unwrap_or(0);
        let delta = bytes.saturating_sub(previous);
        extra = extra.saturating_add(i64::try_from(delta).unwrap_or(i64::MAX));
    }
    extra
}

/// Token status, expiry, and the owning user's ban / deactivation.
///
/// `unlimited` still skips `expires_at`. It does not skip a banned,
/// suspended, or deactivated user. Expiry is `expires_at < now`.
pub async fn token_auth_block(pool: &PgPool, token: &Token) -> Result<Option<AuthBlock>> {
    if token.status != "active" {
        return Ok(Some(AuthBlock::TokenInactive));
    }
    if !token.unlimited {
        if let Some(exp) = token.expires_at {
            if Utc::now() > exp {
                return Ok(Some(AuthBlock::TokenExpired));
            }
        }
    }
    let Some(user_id) = token.user_id else {
        return Ok(None);
    };
    match load_account(pool, user_id).await? {
        Some(view) => Ok(account_block(&view)),
        None => Ok(Some(AuthBlock::Deactivated)),
    }
}

pub fn account_block(view: &AccountView) -> Option<AuthBlock> {
    if view.deactivated_at.is_some() {
        return Some(AuthBlock::Deactivated);
    }
    if view.status != "active" {
        return Some(AuthBlock::Suspended {
            banned: view.status == "banned",
        });
    }
    None
}

/// `true` while the tunnel owner is an unclaimed user still on the agent plan.
///
/// Claim sets `claimed_at`. Upgrade points the subscription at another plan.
/// Either change clears this with no further code change.
pub fn unclaimed_agent_tunnel(view: &AccountView) -> bool {
    view.kind == "agent"
        && view.claimed_at.is_none()
        && view
            .plan
            .as_ref()
            .is_some_and(|plan| plan.name == AgentPlanLimits::NAME)
}

/// Plan-row registration gate.
///
/// Protocol, then a custom subdomain, then the concurrent-tunnel cap, then the
/// hard bandwidth cap. Hobby (`billing_model = "free"`) does not get the
/// subdomain or tunnel-count denial here — [`crate::billing`] already speaks
/// those in the released CLI's upgrade copy. A disallowed protocol is still
/// reported first, including for Hobby, whose seeded list contains every
/// protocol so the check does not fire.
pub fn plan_registration_denial(
    plan: &PlanLimits,
    protocol: &str,
    requested_subdomain: Option<&str>,
    open_tunnels: i64,
    month_bytes: i64,
) -> Option<RegistrationDenial> {
    let protocol = protocol.trim();
    if !protocol.is_empty() && !protocol_allowed(&plan.allowed_protocols, protocol) {
        return Some(RegistrationDenial {
            code: code::PROTOCOL_NOT_ALLOWED,
            message: format!("{} plan does not allow {protocol} tunnels", plan.name),
        });
    }
    let hobby = plan.billing_model == "free";
    if !hobby && subdomain_requested(requested_subdomain) && !plan.allow_custom_subdomains {
        return Some(RegistrationDenial {
            code: code::CUSTOM_SUBDOMAIN_NOT_ALLOWED,
            message: format!("{} plan does not allow a custom subdomain", plan.name),
        });
    }
    if !hobby {
        if let Some(limit) = plan.max_tunnels {
            if open_tunnels >= i64::from(limit) {
                let noun = if limit == 1 { "tunnel" } else { "tunnels" };
                return Some(RegistrationDenial {
                    code: code::TUNNEL_LIMIT,
                    message: format!("{} plan allows {limit} concurrent {noun}", plan.name),
                });
            }
        }
    }
    if let Some(limit) = hard_cap_limit(plan) {
        if month_bytes >= limit {
            return Some(RegistrationDenial {
                code: code::BANDWIDTH_CAP_REACHED,
                message: format!(
                    "{} plan monthly bandwidth cap reached ({limit} bytes)",
                    plan.name
                ),
            });
        }
    }
    None
}

fn protocol_allowed(allowed: &[String], protocol: &str) -> bool {
    allowed
        .iter()
        .any(|name| name.eq_ignore_ascii_case(protocol))
}

fn subdomain_requested(requested: Option<&str>) -> bool {
    requested
        .map(str::trim)
        .is_some_and(|name| !name.is_empty())
}

fn hard_cap_limit(plan: &PlanLimits) -> Option<i64> {
    if plan.bandwidth_hard_cap {
        plan.bandwidth_limit_bytes.filter(|limit| *limit > 0)
    } else {
        None
    }
}

/// Load the active plan and apply [`plan_registration_denial`].
///
/// No active subscription means no plan gate (admin, legacy, and direct
/// tokens). Hobby returns `None` unless the row's protocol list or hard cap
/// actually rejects the attempt.
pub async fn evaluate_plan_registration(
    pool: &PgPool,
    user_id: Uuid,
    protocol: &str,
    requested_subdomain: Option<&str>,
    core: &TunnelCore,
) -> Result<Option<RegistrationDenial>> {
    let Some(view) = load_account(pool, user_id).await? else {
        return Ok(None);
    };
    let Some(plan) = view.plan else {
        return Ok(None);
    };
    // Protocol first, before the count and byte queries.
    if let Some(denial) = plan_registration_denial(&plan, protocol, requested_subdomain, 0, 0) {
        if denial.code == code::PROTOCOL_NOT_ALLOWED
            || denial.code == code::CUSTOM_SUBDOMAIN_NOT_ALLOWED
        {
            return Ok(Some(denial));
        }
    }
    let hobby = plan.billing_model == "free";
    let open_tunnels = if hobby || plan.max_tunnels.is_none() {
        0
    } else {
        count_open_tunnels(pool, user_id).await?
    };
    let month_bytes = if hard_cap_limit(&plan).is_some() {
        // The open row stays at 0 until the meter flush or close. Add the
        // counters the proxy has already incremented.
        month_bytes_including_live(pool, user_id, &core.live_bytes_proxied_for_user(user_id))
            .await?
    } else {
        0
    };
    Ok(plan_registration_denial(
        &plan,
        protocol,
        requested_subdomain,
        open_tunnels,
        month_bytes,
    ))
}

async fn count_open_tunnels(pool: &PgPool, user_id: Uuid) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM tunnel_log \
         WHERE user_id = $1 AND unregistered_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: Uuid,
    status: String,
    kind: String,
    claimed_at: Option<DateTime<Utc>>,
    deactivated_at: Option<DateTime<Utc>>,
    plan_name: Option<String>,
    billing_model: Option<String>,
    max_tunnels: Option<i32>,
    allow_custom_subdomains: Option<bool>,
    allowed_protocols: Option<Vec<String>>,
    bandwidth_hard_cap: Option<bool>,
    bandwidth_limit_bytes: Option<i64>,
}

const ACCOUNT_SELECT: &str = "SELECT u.id, u.status, u.kind, u.claimed_at, u.deactivated_at, \
     p.name AS plan_name, p.billing_model, p.max_tunnels, \
     p.allow_custom_subdomains, p.allowed_protocols, \
     p.bandwidth_hard_cap, p.bandwidth_limit_bytes \
     FROM users u \
     LEFT JOIN LATERAL ( \
         SELECT pl.name, pl.billing_model, pl.max_tunnels, \
                pl.allow_custom_subdomains, pl.allowed_protocols, \
                pl.bandwidth_hard_cap, pl.bandwidth_limit_bytes \
         FROM subscriptions s \
         JOIN plans pl ON pl.id = s.plan_id \
         WHERE s.user_id = u.id AND s.status = 'active' \
         ORDER BY s.created_at DESC \
         LIMIT 1 \
     ) p ON TRUE";

fn view_from_row(row: AccountRow) -> AccountView {
    let plan = row.plan_name.map(|name| PlanLimits {
        name,
        billing_model: row.billing_model.unwrap_or_default(),
        max_tunnels: row.max_tunnels,
        allow_custom_subdomains: row.allow_custom_subdomains.unwrap_or(false),
        allowed_protocols: row.allowed_protocols.unwrap_or_default(),
        bandwidth_hard_cap: row.bandwidth_hard_cap.unwrap_or(false),
        bandwidth_limit_bytes: row.bandwidth_limit_bytes,
    });
    AccountView {
        status: row.status,
        kind: row.kind,
        claimed_at: row.claimed_at,
        deactivated_at: row.deactivated_at,
        plan,
    }
}

async fn load_account(pool: &PgPool, user_id: Uuid) -> Result<Option<AccountView>> {
    let row: Option<AccountRow> = sqlx::query_as(&format!("{ACCOUNT_SELECT} WHERE u.id = $1"))
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(view_from_row))
}

/// GET navigations that ask for HTML. `Sec-Fetch-Mode` must be `navigate`
/// when the browser sent it. `*/*` (curl) is not HTML.
pub fn is_browser_navigation(
    method: &str,
    accept: Option<&str>,
    sec_fetch_mode: Option<&str>,
) -> bool {
    if !method.eq_ignore_ascii_case("GET") {
        return false;
    }
    let Some(accept) = accept else {
        return false;
    };
    if !accept.to_ascii_lowercase().contains("text/html") {
        return false;
    }
    !matches!(
        sec_fetch_mode.map(str::trim),
        Some(mode) if !mode.eq_ignore_ascii_case("navigate")
    )
}

pub fn warning_skipped(skip_header: Option<&str>, cookie: Option<&str>) -> bool {
    skip_header.map(str::trim) == Some("1") || cookie_has_warning_ack(cookie)
}

pub fn cookie_has_warning_ack(cookie: Option<&str>) -> bool {
    let Some(cookie) = cookie else {
        return false;
    };
    cookie.split(';').any(|part| {
        let mut kv = part.trim().splitn(2, '=');
        let name = kv.next().unwrap_or("");
        let value = kv.next().map(str::trim).unwrap_or("");
        name == WARNING_COOKIE_NAME && value == "1"
    })
}

/// Continue link target. `None` when this request is not the acknowledgement.
///
/// The caller must invoke this only for a GET on an unclaimed agent tunnel.
/// Any other request, including one that carries the query flag, is proxied
/// with that flag still on it. The returned path drops only
/// [`WARNING_ACK_PARAM`] and stays on this host.
pub fn warning_acknowledgement(path_and_query: &str, https: bool) -> Option<EdgeResponse> {
    let location = strip_ack_param(path_and_query)?;
    Some(EdgeResponse {
        status: 303,
        content_type: "text/plain; charset=utf-8",
        body: String::new(),
        set_cookie: Some(warning_cookie(https)),
        location: Some(location),
    })
}

pub fn warning_cookie(https: bool) -> String {
    let mut cookie = format!(
        "{WARNING_COOKIE_NAME}=1; Path=/; Max-Age={WARNING_COOKIE_MAX_AGE_SECS}; SameSite=Lax; HttpOnly"
    );
    if https {
        cookie.push_str("; Secure");
    }
    cookie
}

fn strip_ack_param(path_and_query: &str) -> Option<String> {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    if query.is_empty() {
        return None;
    }
    let mut kept = Vec::new();
    let mut found = false;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == WARNING_ACK_PARAM && (value == "1" || value.is_empty()) {
            found = true;
            continue;
        }
        kept.push(pair);
    }
    if !found {
        return None;
    }
    let path = safe_path(path);
    if kept.is_empty() {
        Some(path.to_string())
    } else {
        Some(format!("{path}?{}", kept.join("&")))
    }
}

/// Same-origin path, or `/` when `path` could leave this host.
///
/// A browser treats `/\evil.com` as `//evil.com`. `%5C` is that backslash
/// once decoded, in either case. A path has to start with one `/` and no
/// second slash.
fn safe_path(path: &str) -> &str {
    if is_same_origin_path(path) {
        path
    } else {
        "/"
    }
}

fn is_same_origin_path(path: &str) -> bool {
    let mut chars = path.chars();
    if chars.next() != Some('/') {
        return false;
    }
    // A second slash is a protocol-relative URL. A backslash is the same
    // thing after a browser normalizes it.
    if matches!(chars.next(), Some('/') | Some('\\')) {
        return false;
    }
    if path.contains('\\') || path.to_ascii_lowercase().contains("%5c") {
        return false;
    }
    true
}

/// Ban, hard cap, or the unclaimed-agent warning. `Proxy` means forward.
pub fn edge_action(view: &AccountView, month_bytes: i64, req: &IncomingRequest<'_>) -> EdgeAction {
    if let Some(block) = account_block(view) {
        return EdgeAction::Respond(account_page(block, req));
    }
    if let Some(plan) = view.plan.as_ref() {
        if let Some(limit) = hard_cap_limit(plan) {
            if month_bytes >= limit {
                return EdgeAction::Respond(bandwidth_page(plan, limit, req));
            }
        }
    }
    // The continue flag is handled only for GET on an unclaimed agent
    // tunnel. A human tunnel, a POST, or any other request keeps the query
    // string and is proxied.
    if req.method.eq_ignore_ascii_case("GET") && unclaimed_agent_tunnel(view) {
        if let Some(ack) = warning_acknowledgement(req.path_and_query, req.https) {
            return EdgeAction::Respond(ack);
        }
    }
    if unclaimed_agent_tunnel(view)
        && !warning_skipped(req.skip_warning, req.cookie)
        && is_browser_navigation(req.method, req.accept, req.sec_fetch_mode)
    {
        return EdgeAction::Respond(warning_page(req.host, req.path_and_query));
    }
    EdgeAction::Proxy
}

fn account_page(block: AuthBlock, req: &IncomingRequest<'_>) -> EdgeResponse {
    let message = block.message();
    if is_browser_navigation(req.method, req.accept, req.sec_fetch_mode) {
        let body = html_doc(
            "Tunnel unavailable",
            &format!(
                "<h1>Tunnel unavailable</h1>\
                 <p>{}</p>",
                escape_html(message)
            ),
        );
        html_response(403, body)
    } else {
        json_response(403, block.code(), message)
    }
}

fn bandwidth_page(plan: &PlanLimits, limit: i64, req: &IncomingRequest<'_>) -> EdgeResponse {
    let message = format!(
        "{} plan monthly bandwidth cap reached ({limit} bytes)",
        plan.name
    );
    if is_browser_navigation(req.method, req.accept, req.sec_fetch_mode) {
        let body = html_doc(
            "Bandwidth cap reached",
            &format!(
                "<h1>Bandwidth cap reached</h1>\
                 <p>{}</p>\
                 <p>Further traffic on this tunnel is blocked until the next UTC month or the plan changes.</p>",
                escape_html(&message)
            ),
        );
        html_response(402, body)
    } else {
        json_response(402, code::BANDWIDTH_CAP_REACHED, &message)
    }
}

fn warning_page(host: &str, path_and_query: &str) -> EdgeResponse {
    let href = continue_href(path_and_query);
    let body = html_doc(
        "Temporary unverified tunnel",
        &format!(
            "<h1>Temporary unverified tunnel</h1>\
             <p>This address is a temporary tunnel created by an automated agent. \
                The account behind it has not been claimed.</p>\
             <p>Host: <span class=\"host\">{}</span></p>\
             <p class=\"warn\">Do not enter passwords, payment details, or other secrets on this site.</p>\
             <p><a class=\"continue\" href=\"{}\">Continue</a></p>",
            escape_html(host),
            escape_html(&href)
        ),
    );
    html_response(200, body)
}

fn continue_href(path_and_query: &str) -> String {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let path = safe_path(path);
    let extra = format!("{WARNING_ACK_PARAM}=1");
    if query.is_empty() {
        format!("{path}?{extra}")
    } else {
        format!("{path}?{query}&{extra}")
    }
}

fn html_response(status: u16, body: String) -> EdgeResponse {
    EdgeResponse {
        status,
        content_type: "text/html; charset=utf-8",
        body,
        set_cookie: None,
        location: None,
    }
}

fn json_response(status: u16, error_code: &str, message: &str) -> EdgeResponse {
    EdgeResponse {
        status,
        content_type: "application/json",
        body: json!({ "code": error_code, "error": message }).to_string(),
        set_cookie: None,
        location: None,
    }
}

fn html_doc(title: &str, main: &str) -> String {
    format!(
        "<!DOCTYPE html>\
         <html lang=\"en\">\
         <head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title>\
         <style>\
         body{{font-family:sans-serif;background:#fff;color:#1a1a1a;margin:0}}\
         main{{max-width:36rem;margin:4rem auto;padding:0 1.25rem}}\
         h1{{font-size:1.4rem}}\
         .host{{font-family:ui-monospace,monospace;background:#f4f4f4;padding:0.15rem 0.4rem}}\
         .continue{{display:inline-block;margin-top:0.5rem;background:#111;color:#fff;\
           padding:0.6rem 1rem;text-decoration:none;border-radius:4px}}\
         .warn{{border-left:4px solid #b45309;padding-left:0.75rem}}\
         </style>\
         </head>\
         <body><main>{main}</main></body></html>",
        escape_html(title)
    )
}

pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Mark expired unclaimed agent accounts. One statement, safe on every replica.
///
/// Matches `kind = 'agent' AND claimed_at IS NULL AND deactivated_at IS NULL`
/// whose every token has `expires_at < now()`. A token with `expires_at`
/// NULL has not expired, so the account is left alone. Accounts with no
/// tokens are left alone. Rows and tokens are not deleted.
pub async fn deactivate_expired_unclaimed_agents(pool: &PgPool) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE users AS u \
         SET deactivated_at = NOW() \
         WHERE u.kind = 'agent' \
           AND u.claimed_at IS NULL \
           AND u.deactivated_at IS NULL \
           AND EXISTS (SELECT 1 FROM tokens t WHERE t.user_id = u.id) \
           AND NOT EXISTS ( \
               SELECT 1 FROM tokens t \
               WHERE t.user_id = u.id \
                 AND (t.expires_at IS NULL OR t.expires_at >= NOW()) \
           )",
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

#[derive(sqlx::FromRow)]
struct LiveToken {
    id: String,
    status: String,
    expires_at: Option<DateTime<Utc>>,
    unlimited: bool,
    user_id: Option<Uuid>,
}

fn token_blocks(token: &LiveToken) -> bool {
    if token.status != "active" {
        return true;
    }
    if !token.unlimited {
        if let Some(exp) = token.expires_at {
            if Utc::now() > exp {
                return true;
            }
        }
    }
    false
}

/// `true` when this user's hard cap is already spent.
///
/// The total is the committed month sum plus unflushed counters
/// ([`AccountGate::month_bytes`]). A database error with a cached total still
/// enforces the cap. With no cached total the session stays up.
async fn over_hard_cap(
    gate: &AccountGate,
    core: &TunnelCore,
    view: &AccountView,
    user_id: Uuid,
) -> bool {
    let Some(plan) = view.plan.as_ref() else {
        return false;
    };
    let Some(limit) = hard_cap_limit(plan) else {
        return false;
    };
    match gate.month_bytes(user_id, core).await {
        Ok(bytes) => bytes >= limit,
        Err(e) => {
            tracing::warn!(
                error = %e,
                %user_id,
                "bandwidth recheck failed with no cache; leaving session up"
            );
            false
        }
    }
}

/// Tokens and their users, one round trip, bounded by [`STATEMENT_TIMEOUT`].
async fn load_recheck_rows(
    pool: &PgPool,
    token_ids: &[String],
) -> Result<(
    std::collections::HashMap<String, LiveToken>,
    std::collections::HashMap<Uuid, AccountView>,
)> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = '{STATEMENT_TIMEOUT}'"
    ))
    .execute(&mut *tx)
    .await?;
    let tokens: Vec<LiveToken> = sqlx::query_as(
        "SELECT id, status, expires_at, unlimited, user_id \
         FROM tokens WHERE id = ANY($1)",
    )
    .bind(token_ids)
    .fetch_all(&mut *tx)
    .await?;
    let user_ids: Vec<Uuid> = tokens.iter().filter_map(|token| token.user_id).collect();
    let accounts = if user_ids.is_empty() {
        Vec::new()
    } else {
        let sql = format!("{ACCOUNT_SELECT} WHERE u.id = ANY($1)");
        sqlx::query_as::<_, AccountRow>(&sql)
            .bind(&user_ids)
            .fetch_all(&mut *tx)
            .await?
    };
    tx.commit().await?;
    let tokens = tokens
        .into_iter()
        .map(|token| (token.id.clone(), token))
        .collect();
    let accounts = accounts
        .into_iter()
        .map(|row| {
            let id = row.id;
            (id, view_from_row(row))
        })
        .collect();
    Ok((tokens, accounts))
}

/// Close sessions whose token, user, or hard bandwidth cap says to stop.
///
/// Token and account rows are loaded with `id = ANY($1)` under
/// [`STATEMENT_TIMEOUT`], so one stuck database cannot stall the loop for
/// every session. The hard cap adds this process's unflushed counters
/// ([`TunnelCore::live_bytes_proxied_for_user`], the same totals
/// [`TunnelCore::get_tunnel_bytes_proxied`] reports) to the committed month
/// sum. A database error uses `gate`'s last account and byte snapshot,
/// including one past its TTL. With no snapshot the session stays up.
/// Sends [`ControlMessage::Shutdown`]; the session task drops its tunnels.
pub async fn disconnect_blocked_sessions(
    pool: &PgPool,
    core: &TunnelCore,
    gate: &AccountGate,
) -> Result<usize> {
    let snapshot: Vec<(Uuid, String)> = core
        .sessions
        .iter()
        .filter_map(|entry| {
            entry
                .db_token_id
                .clone()
                .map(|token_id| (*entry.key(), token_id))
        })
        .collect();
    if snapshot.is_empty() {
        return Ok(0);
    }
    let token_ids: Vec<String> = snapshot.iter().map(|(_, id)| id.clone()).collect();
    match load_recheck_rows(pool, &token_ids).await {
        Ok((tokens, accounts)) => {
            let mut closed = 0usize;
            let mut over_cap: std::collections::HashMap<Uuid, bool> =
                std::collections::HashMap::new();
            for (session_id, token_id) in snapshot {
                let close = match tokens.get(&token_id) {
                    None => true,
                    Some(token) if token_blocks(token) => true,
                    Some(token) => match token.user_id {
                        None => false,
                        Some(user_id) => match accounts.get(&user_id) {
                            None => true,
                            Some(view) if account_block(view).is_some() => true,
                            Some(view) => match over_cap.get(&user_id).copied() {
                                Some(over) => over,
                                None => {
                                    let over = over_hard_cap(gate, core, view, user_id).await;
                                    over_cap.insert(user_id, over);
                                    over
                                }
                            },
                        },
                    },
                };
                if close && shutdown_session(core, session_id) {
                    closed += 1;
                }
            }
            Ok(closed)
        }
        Err(e) => {
            let mut closed = 0usize;
            let mut over_cap: std::collections::HashMap<Uuid, bool> =
                std::collections::HashMap::new();
            for (session_id, _) in snapshot {
                let user_id = core
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.user_id);
                let Some(user_id) = user_id else {
                    tracing::warn!(
                        error = %e,
                        %session_id,
                        "session recheck failed with no cache; leaving session up"
                    );
                    continue;
                };
                let Some(view) = gate.any_view(user_id) else {
                    tracing::warn!(
                        error = %e,
                        %session_id,
                        "session recheck failed with no cache; leaving session up"
                    );
                    continue;
                };
                tracing::warn!(
                    error = %e,
                    %session_id,
                    "session recheck failed; using cached account"
                );
                let close = account_block(&view).is_some()
                    || match over_cap.get(&user_id).copied() {
                        Some(over) => over,
                        None => {
                            let over = over_hard_cap(gate, core, &view, user_id).await;
                            over_cap.insert(user_id, over);
                            over
                        }
                    };
                if close && shutdown_session(core, session_id) {
                    closed += 1;
                }
            }
            Ok(closed)
        }
    }
}

fn shutdown_session(core: &TunnelCore, session_id: Uuid) -> bool {
    let sent = core.sessions.get(&session_id).is_some_and(|session| {
        session
            .control_tx
            .try_send(ControlMessage::Shutdown)
            .is_ok()
    });
    if sent {
        tracing::info!(%session_id, "closing session after account recheck");
    }
    sent
}

/// Sweep plus session recheck. Runs until the task is aborted on shutdown.
/// The sweep's first tick is immediate, so a restart deactivates before the
/// full interval.
pub async fn run_account_maintenance(
    pool: PgPool,
    core: std::sync::Arc<TunnelCore>,
    gate: std::sync::Arc<AccountGate>,
) {
    let mut sweep = tokio::time::interval(DEACTIVATION_SWEEP_INTERVAL);
    let mut sessions = tokio::time::interval(SESSION_RECHECK_INTERVAL);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    sessions.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = sweep.tick() => {
                match deactivate_expired_unclaimed_agents(&pool).await {
                    Ok(n) if n > 0 => tracing::info!(count = n, "deactivated expired unclaimed agents"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "agent deactivation sweep failed"),
                }
            }
            _ = sessions.tick() => {
                if let Err(e) = disconnect_blocked_sessions(&pool, &core, &gate).await {
                    tracing::warn!(error = %e, "session access recheck failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseSection;
    use crate::db::init_db;

    fn agent_plan() -> PlanLimits {
        PlanLimits {
            name: AgentPlanLimits::NAME.into(),
            billing_model: AgentPlanLimits::BILLING_MODEL.into(),
            max_tunnels: Some(AgentPlanLimits::MAX_TUNNELS),
            allow_custom_subdomains: AgentPlanLimits::ALLOW_CUSTOM_SUBDOMAINS,
            allowed_protocols: AgentPlanLimits::ALLOWED_PROTOCOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            bandwidth_hard_cap: AgentPlanLimits::BANDWIDTH_HARD_CAP,
            bandwidth_limit_bytes: Some(AgentPlanLimits::BANDWIDTH_LIMIT_BYTES),
        }
    }

    fn hobby_plan() -> PlanLimits {
        PlanLimits {
            name: "free".into(),
            billing_model: "free".into(),
            max_tunnels: Some(2),
            allow_custom_subdomains: false,
            allowed_protocols: ["http", "https", "tcp", "udp", "p2p"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            bandwidth_hard_cap: false,
            bandwidth_limit_bytes: None,
        }
    }

    fn payg_plan() -> PlanLimits {
        PlanLimits {
            name: "payg".into(),
            billing_model: "metered".into(),
            max_tunnels: None,
            allow_custom_subdomains: true,
            allowed_protocols: ["http", "https", "tcp", "udp", "p2p"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            bandwidth_hard_cap: false,
            bandwidth_limit_bytes: None,
        }
    }

    #[test]
    fn protocol_is_reported_before_subdomain_tunnel_cap_and_bandwidth() {
        let plan = agent_plan();
        let denied = plan_registration_denial(&plan, "tcp", Some("myapp"), 5, i64::MAX).unwrap();
        assert_eq!(denied.code, code::PROTOCOL_NOT_ALLOWED);
        assert_eq!(denied.message, "agent plan does not allow tcp tunnels");
        for protocol in ["udp", "p2p", "TCP"] {
            let denied = plan_registration_denial(&plan, protocol, None, 0, 0).unwrap();
            assert_eq!(denied.code, code::PROTOCOL_NOT_ALLOWED, "{protocol}");
        }
        assert!(plan_registration_denial(&plan, "https", None, 0, 0).is_none());
        assert!(plan_registration_denial(&plan, "HTTP", Some("  "), 0, 0).is_none());
    }

    #[test]
    fn custom_subdomain_and_tunnel_limit_use_the_plan_row() {
        let plan = agent_plan();
        let sub = plan_registration_denial(&plan, "http", Some("myapp"), 0, 0).unwrap();
        assert_eq!(sub.code, code::CUSTOM_SUBDOMAIN_NOT_ALLOWED);
        assert_eq!(sub.message, "agent plan does not allow a custom subdomain");

        let second = plan_registration_denial(&plan, "http", None, 1, 0).unwrap();
        assert_eq!(second.code, code::TUNNEL_LIMIT);
        assert_eq!(second.message, "agent plan allows 1 concurrent tunnel");

        let cap = plan_registration_denial(
            &plan,
            "http",
            None,
            0,
            AgentPlanLimits::BANDWIDTH_LIMIT_BYTES,
        )
        .unwrap();
        assert_eq!(cap.code, code::BANDWIDTH_CAP_REACHED);
        assert!(cap
            .message
            .contains(&AgentPlanLimits::BANDWIDTH_LIMIT_BYTES.to_string()));
    }

    #[test]
    fn hobby_and_payg_keep_their_existing_registration_behavior() {
        let hobby = hobby_plan();
        assert!(plan_registration_denial(&hobby, "tcp", Some("myapp"), 9, i64::MAX).is_none());
        let payg = payg_plan();
        assert!(plan_registration_denial(&payg, "udp", Some("myapp"), 40, i64::MAX).is_none());
    }

    #[test]
    fn hard_cap_follows_the_flag_not_the_plan_name() {
        let mut plan = payg_plan();
        plan.bandwidth_hard_cap = true;
        plan.bandwidth_limit_bytes = Some(100);
        let denied = plan_registration_denial(&plan, "http", None, 0, 100).unwrap();
        assert_eq!(denied.code, code::BANDWIDTH_CAP_REACHED);
        assert!(denied.message.starts_with("payg plan"));
        assert!(plan_registration_denial(&plan, "http", None, 0, 99).is_none());
    }

    fn view(kind: &str, claimed: bool, plan_name: &str) -> AccountView {
        AccountView {
            status: "active".into(),
            kind: kind.into(),
            claimed_at: claimed.then_some(Utc::now()),
            deactivated_at: None,
            plan: Some(PlanLimits {
                name: plan_name.into(),
                billing_model: if plan_name == "agent" {
                    "agent"
                } else {
                    "metered"
                }
                .into(),
                max_tunnels: Some(1),
                allow_custom_subdomains: false,
                allowed_protocols: vec!["http".into()],
                bandwidth_hard_cap: false,
                bandwidth_limit_bytes: None,
            }),
        }
    }

    fn browser<'a>(host: &'a str, path: &'a str) -> IncomingRequest<'a> {
        IncomingRequest {
            method: "GET",
            accept: Some("text/html,application/xhtml+xml"),
            sec_fetch_mode: Some("navigate"),
            skip_warning: None,
            cookie: None,
            host,
            path_and_query: path,
            https: true,
        }
    }

    #[test]
    fn warning_tracks_kind_claim_and_the_live_plan() {
        let req = browser("demo.example", "/");
        match edge_action(&view("agent", false, "agent"), 0, &req) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 200);
                assert!(page.body.contains("automated agent"));
                assert!(page.body.contains("demo.example"));
                assert!(page.body.contains("passwords"));
                assert!(page.body.contains("rustunnel_warn_continue=1"));
                assert!(!page.body.contains("http://"));
                assert!(!page.body.contains("https://"));
                assert!(!page.body.contains("src="));
            }
            EdgeAction::Proxy => panic!("expected the warning"),
        }
        assert!(matches!(
            edge_action(&view("agent", true, "agent"), 0, &req),
            EdgeAction::Proxy
        ));
        assert!(matches!(
            edge_action(&view("agent", false, "payg"), 0, &req),
            EdgeAction::Proxy
        ));
        assert!(matches!(
            edge_action(&view("human", false, "free"), 0, &req),
            EdgeAction::Proxy
        ));
    }

    #[test]
    fn warning_is_only_for_browser_gets() {
        let agent = view("agent", false, "agent");
        let mut curl = browser("h.example", "/path");
        curl.accept = Some("*/*");
        curl.sec_fetch_mode = None;
        assert!(matches!(edge_action(&agent, 0, &curl), EdgeAction::Proxy));

        let mut json = browser("h.example", "/");
        json.accept = Some("application/json");
        assert!(matches!(edge_action(&agent, 0, &json), EdgeAction::Proxy));

        let mut post = browser("h.example", "/");
        post.method = "POST";
        assert!(matches!(edge_action(&agent, 0, &post), EdgeAction::Proxy));

        let mut cors = browser("h.example", "/");
        cors.sec_fetch_mode = Some("cors");
        assert!(matches!(edge_action(&agent, 0, &cors), EdgeAction::Proxy));

        let mut skip = browser("h.example", "/");
        skip.skip_warning = Some("1");
        assert!(matches!(edge_action(&agent, 0, &skip), EdgeAction::Proxy));

        let mut acked = browser("h.example", "/");
        acked.cookie = Some("other=1; rustunnel_warn_ok=1");
        assert!(matches!(edge_action(&agent, 0, &acked), EdgeAction::Proxy));
    }

    #[test]
    fn warning_page_escapes_the_host() {
        let req = browser("<script>alert(1)</script>", "/?q=\"x\"");
        match edge_action(&view("agent", false, "agent"), 0, &req) {
            EdgeAction::Respond(page) => {
                assert!(!page.body.contains("<script>"));
                assert!(page.body.contains("&lt;script&gt;"));
                assert!(page.body.contains("&quot;"));
                assert!(page.body.contains("rustunnel_warn_continue=1"));
            }
            EdgeAction::Proxy => panic!("expected the warning"),
        }
    }

    #[test]
    fn continue_sets_a_host_scoped_cookie_and_strips_only_the_ack() {
        let ack = warning_acknowledgement("/docs?x=1&rustunnel_warn_continue=1", true).unwrap();
        assert_eq!(ack.status, 303);
        assert_eq!(ack.location.as_deref(), Some("/docs?x=1"));
        let cookie = ack.set_cookie.unwrap();
        assert!(cookie.starts_with("rustunnel_warn_ok=1; Path=/;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(!cookie.contains("Domain="));
        assert!(warning_acknowledgement("/docs?x=1", false).is_none());
        assert_eq!(
            warning_acknowledgement("/?rustunnel_warn_continue=1", false)
                .unwrap()
                .location
                .as_deref(),
            Some("/")
        );
    }

    #[test]
    fn continue_stays_on_this_host() {
        for path in [
            "/\\evil.com",
            "/%5Cevil.com",
            "/%5cevil.com",
            "//evil.com",
            "https://evil.com",
        ] {
            let ack = warning_acknowledgement(&format!("{path}?rustunnel_warn_continue=1"), false)
                .unwrap();
            let location = ack.location.unwrap();
            assert_eq!(location, "/", "{path} redirected to {location}");
        }
        let kept = warning_acknowledgement("/docs?rustunnel_warn_continue=1&x=1", false).unwrap();
        assert_eq!(kept.location.as_deref(), Some("/docs?x=1"));
    }

    #[test]
    fn continue_is_ignored_unless_this_is_an_unclaimed_agent_get() {
        let agent = view("agent", false, "agent");
        let human = view("human", false, "free");
        let flagged = "/\\evil.com?rustunnel_warn_continue=1";

        let mut post = browser("h.example", flagged);
        post.method = "POST";
        assert!(
            matches!(edge_action(&agent, 0, &post), EdgeAction::Proxy),
            "a POST carrying the flag is proxied"
        );

        let human_get = browser("h.example", "/?rustunnel_warn_continue=1");
        assert!(
            matches!(edge_action(&human, 0, &human_get), EdgeAction::Proxy),
            "a human tunnel carrying the flag is proxied"
        );

        match edge_action(&agent, 0, &browser("h.example", flagged)) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 303);
                assert_eq!(page.location.as_deref(), Some("/"));
            }
            EdgeAction::Proxy => panic!("an unclaimed agent GET should acknowledge"),
        }
    }

    #[test]
    fn bandwidth_and_ban_pages_use_stable_codes() {
        let mut plan = agent_plan();
        plan.bandwidth_hard_cap = true;
        let mut account = view("agent", false, "agent");
        account.plan = Some(plan);
        let curl = IncomingRequest {
            method: "GET",
            accept: Some("*/*"),
            sec_fetch_mode: None,
            skip_warning: None,
            cookie: None,
            host: "h.example",
            path_and_query: "/",
            https: false,
        };
        match edge_action(&account, AgentPlanLimits::BANDWIDTH_LIMIT_BYTES, &curl) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 402);
                assert!(page.body.contains(code::BANDWIDTH_CAP_REACHED));
                assert_eq!(page.content_type, "application/json");
            }
            EdgeAction::Proxy => panic!("expected the cap"),
        }
        account.status = "banned".into();
        account.deactivated_at = None;
        match edge_action(&account, 0, &curl) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 403);
                assert!(page.body.contains(code::ACCOUNT_SUSPENDED));
                assert!(page.body.contains("account banned"));
            }
            EdgeAction::Proxy => panic!("expected the ban"),
        }
    }

    fn idle_core() -> TunnelCore {
        TunnelCore::new([20000, 20001], [0, 0], 4, 8, 100, 20)
    }

    async fn pool() -> PgPool {
        let url = std::env::var("TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://rustunnel:test@127.0.0.1:5432/rustunnel_test".into());
        init_db(&DatabaseSection {
            url,
            captured_path: ":memory:".into(),
        })
        .await
        .expect("init_db")
        .pg
    }

    struct Fixture {
        user_id: Uuid,
        pool: PgPool,
    }

    impl Fixture {
        async fn insert(
            pool: &PgPool,
            kind: &str,
            plan: &str,
            claimed: bool,
            status: &str,
        ) -> Self {
            let user_id = Uuid::new_v4();
            let claimed_at: Option<DateTime<Utc>> = claimed.then_some(Utc::now());
            sqlx::query(
                "INSERT INTO users (id, email, password_hash, kind, status, auth_method, source, claimed_at) \
                 VALUES ($1, $2, '', $3, $4, 'agent', 'api', $5)",
            )
            .bind(user_id)
            .bind(format!("{kind}-{user_id}@example.com"))
            .bind(kind)
            .bind(status)
            .bind(claimed_at)
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO subscriptions (user_id, plan_id, status) \
                 SELECT $1, id, 'active' FROM plans WHERE name = $2",
            )
            .bind(user_id)
            .bind(plan)
            .execute(pool)
            .await
            .unwrap();
            Self {
                user_id,
                pool: pool.clone(),
            }
        }

        async fn cleanup(self) {
            let _ = sqlx::query("DELETE FROM tunnel_log WHERE user_id = $1")
                .bind(self.user_id)
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM tokens WHERE user_id = $1")
                .bind(self.user_id)
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM subscriptions WHERE user_id = $1")
                .bind(self.user_id)
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(self.user_id)
                .execute(&self.pool)
                .await;
        }
    }

    async fn add_token(pool: &PgPool, user_id: Uuid, expires_at: Option<DateTime<Utc>>) {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO tokens (id, token_hash, label, created_at, user_id, expires_at, status) \
             VALUES ($1, $2, 't', NOW(), $3, $4, 'active')",
        )
        .bind(&id)
        .bind(format!("hash-{id}"))
        .bind(user_id)
        .bind(expires_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn live_agent_plan_row_drives_registration() {
        let pool = pool().await;
        let core = idle_core();
        let fixture = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let none = evaluate_plan_registration(&pool, fixture.user_id, "http", None, &core)
            .await
            .unwrap();
        assert!(none.is_none());
        let tcp = evaluate_plan_registration(&pool, fixture.user_id, "tcp", Some("name"), &core)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tcp.code, code::PROTOCOL_NOT_ALLOWED);
        let sub = evaluate_plan_registration(&pool, fixture.user_id, "http", Some("myapp"), &core)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub.code, code::CUSTOM_SUBDOMAIN_NOT_ALLOWED);

        let tunnel_id = Uuid::new_v4().to_string();
        crate::db::log_tunnel_registered(
            &pool,
            crate::db::TunnelRegistration {
                tunnel_id: &tunnel_id,
                protocol: "http",
                label: "abcd1234",
                session_id: &Uuid::new_v4().to_string(),
                token_id: None,
                region_id: "test",
                user_id: Some(fixture.user_id),
            },
        )
        .await
        .unwrap();
        let second = evaluate_plan_registration(&pool, fixture.user_id, "https", None, &core)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.code, code::TUNNEL_LIMIT);

        sqlx::query(
            "UPDATE tunnel_log SET unregistered_at = NOW(), \
             bytes_in = $2, bytes_out = $3, bytes_proxied = $2 + $3 \
             WHERE tunnel_id = $1",
        )
        .bind(&tunnel_id)
        .bind(AgentPlanLimits::BANDWIDTH_LIMIT_BYTES)
        .bind(0_i64)
        .execute(&pool)
        .await
        .unwrap();
        let capped = evaluate_plan_registration(&pool, fixture.user_id, "http", None, &core)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(capped.code, code::BANDWIDTH_CAP_REACHED);
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn live_tunnel_bytes_count_before_flush_without_double_counting() {
        use std::sync::atomic::Ordering;

        let pool = pool().await;
        let fixture = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let core = idle_core();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let session_id = core.register_session(
            "127.0.0.1:9".parse().unwrap(),
            "t".into(),
            None,
            Some(fixture.user_id),
            tx,
        );
        let (tunnel_id, subdomain) = core
            .register_http_tunnel(&session_id, None, TunnelProtocol::Http, None)
            .unwrap();
        crate::db::log_tunnel_registered(
            &pool,
            crate::db::TunnelRegistration {
                tunnel_id: &tunnel_id.to_string(),
                protocol: "http",
                label: &subdomain,
                session_id: &session_id.to_string(),
                token_id: None,
                region_id: "test",
                user_id: Some(fixture.user_id),
            },
        )
        .await
        .unwrap();

        let gate = AccountGate::new(pool.clone(), Duration::ZERO, Duration::from_secs(60));
        assert_eq!(gate.month_bytes(fixture.user_id, &core).await.unwrap(), 0);

        let (other_tx, _other_rx) = tokio::sync::mpsc::channel(1);
        let other_session = core.register_session(
            "127.0.0.1:10".parse().unwrap(),
            "other".into(),
            None,
            Some(Uuid::new_v4()),
            other_tx,
        );
        let (_other_id, other_sub) = core
            .register_http_tunnel(&other_session, None, TunnelProtocol::Http, None)
            .unwrap();
        core.http_routes
            .get(&other_sub)
            .unwrap()
            .members
            .iter()
            .next()
            .unwrap()
            .info
            .bytes_in
            .store(50_000_000, Ordering::Relaxed);

        let cap = AgentPlanLimits::BANDWIDTH_LIMIT_BYTES as u64;
        core.http_routes
            .get(&subdomain)
            .unwrap()
            .members
            .get(&tunnel_id)
            .unwrap()
            .info
            .bytes_in
            .store(cap, Ordering::Relaxed);

        assert_eq!(
            gate.month_bytes(fixture.user_id, &core).await.unwrap(),
            AgentPlanLimits::BANDWIDTH_LIMIT_BYTES,
            "a warm cache must still see bytes proxied after the snapshot"
        );
        let stored: i64 =
            sqlx::query_scalar("SELECT bytes_proxied FROM tunnel_log WHERE tunnel_id = $1")
                .bind(tunnel_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, 0, "the cap must trip before the meter flush");

        crate::db::flush_live_meters(&pool, &core).await.unwrap();
        let fresh = AccountGate::new(pool.clone(), Duration::ZERO, Duration::ZERO);
        assert_eq!(
            fresh.month_bytes(fixture.user_id, &core).await.unwrap(),
            AgentPlanLimits::BANDWIDTH_LIMIT_BYTES,
            "flushing the same counters must not count them twice"
        );
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn stale_cache_keeps_limits_when_the_database_fails() {
        use std::sync::atomic::Ordering;

        let pool = pool().await;
        let capped = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let banned = Fixture::insert(&pool, "agent", "agent", false, "banned").await;
        let capped_id = capped.user_id;
        let banned_id = banned.user_id;
        let core = idle_core();
        let (cap_tx, mut cap_rx) = tokio::sync::mpsc::channel(4);
        let cap_session = core.register_session(
            "127.0.0.1:9".parse().unwrap(),
            "cap".into(),
            Some("cap-token".into()),
            Some(capped_id),
            cap_tx,
        );
        let (tunnel_id, subdomain) = core
            .register_http_tunnel(&cap_session, None, TunnelProtocol::Http, None)
            .unwrap();
        let (ban_tx, mut ban_rx) = tokio::sync::mpsc::channel(4);
        core.register_session(
            "127.0.0.1:10".parse().unwrap(),
            "ban".into(),
            Some("ban-token".into()),
            Some(banned_id),
            ban_tx,
        );
        let uncached_id = Uuid::new_v4();
        let (open_tx, mut open_rx) = tokio::sync::mpsc::channel(4);
        core.register_session(
            "127.0.0.1:11".parse().unwrap(),
            "open".into(),
            Some("open-token".into()),
            Some(uncached_id),
            open_tx,
        );

        let ttl = Duration::from_millis(1);
        let gate = AccountGate::new(pool.clone(), ttl, ttl);
        let active = gate.view(capped_id).await.unwrap().unwrap();
        assert!(account_block(&active).is_none());
        assert_eq!(gate.month_bytes(capped_id, &core).await.unwrap(), 0);
        let banned_view = gate.view(banned_id).await.unwrap().unwrap();
        assert!(account_block(&banned_view).is_some());

        core.http_routes
            .get(&subdomain)
            .unwrap()
            .members
            .get(&tunnel_id)
            .unwrap()
            .info
            .bytes_in
            .store(
                AgentPlanLimits::BANDWIDTH_LIMIT_BYTES as u64,
                Ordering::Relaxed,
            );
        tokio::time::sleep(Duration::from_millis(30)).await;
        capped.cleanup().await;
        banned.cleanup().await;
        pool.close().await;

        let still_active = gate.view(capped_id).await.unwrap().unwrap();
        let bytes = gate.month_bytes(capped_id, &core).await.unwrap();
        assert!(bytes >= AgentPlanLimits::BANDWIDTH_LIMIT_BYTES);
        let curl = IncomingRequest {
            method: "GET",
            accept: Some("application/json"),
            sec_fetch_mode: None,
            skip_warning: None,
            cookie: None,
            host: "h.example",
            path_and_query: "/",
            https: false,
        };
        match edge_action(&still_active, bytes, &curl) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 402);
                assert!(page.body.contains(code::BANDWIDTH_CAP_REACHED));
            }
            EdgeAction::Proxy => panic!("stale byte cache must keep the cap"),
        }
        let still_banned = gate.view(banned_id).await.unwrap().unwrap();
        match edge_action(&still_banned, 0, &curl) {
            EdgeAction::Respond(page) => {
                assert_eq!(page.status, 403);
                assert!(page.body.contains("account banned"));
            }
            EdgeAction::Proxy => panic!("stale account cache must keep the ban"),
        }
        let closed = disconnect_blocked_sessions(&pool, &core, &gate)
            .await
            .unwrap();
        assert_eq!(closed, 2, "a stale cache keeps both the ban and the cap");
        assert!(matches!(ban_rx.try_recv(), Ok(ControlMessage::Shutdown)));
        assert!(matches!(cap_rx.try_recv(), Ok(ControlMessage::Shutdown)));
        assert!(open_rx.try_recv().is_err());

        assert!(gate.view(uncached_id).await.is_err());
        assert!(gate.month_bytes(uncached_id, &core).await.is_err());
        assert!(
            evaluate_plan_registration(&pool, capped_id, "http", None, &core)
                .await
                .is_err(),
            "registration stays fail-closed"
        );
        let token = crate::db::models::Token {
            id: "t".into(),
            token_hash: "h".into(),
            label: "l".into(),
            created_at: Utc::now(),
            last_used_at: None,
            scope: None,
            user_id: Some(capped_id),
            expires_at: None,
            tier: None,
            tunnel_limit: None,
            status: "active".into(),
            unlimited: false,
        };
        assert!(
            token_auth_block(&pool, &token).await.is_err(),
            "token auth stays fail-closed"
        );
    }

    #[tokio::test]
    async fn recheck_closes_a_session_when_live_bytes_cross_the_cap() {
        use std::sync::atomic::Ordering;

        let pool = pool().await;
        let over = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let under = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let over_token = Uuid::new_v4().to_string();
        let under_token = Uuid::new_v4().to_string();
        for (id, user_id) in [(&over_token, over.user_id), (&under_token, under.user_id)] {
            sqlx::query(
                "INSERT INTO tokens (id, token_hash, label, created_at, user_id, status) \
                 VALUES ($1, $2, 't', NOW(), $3, 'active')",
            )
            .bind(id)
            .bind(format!("hash-{id}"))
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let core = idle_core();
        let (over_tx, mut over_rx) = tokio::sync::mpsc::channel(4);
        let over_session = core.register_session(
            "127.0.0.1:9".parse().unwrap(),
            "over".into(),
            Some(over_token),
            Some(over.user_id),
            over_tx,
        );
        let (tunnel_id, subdomain) = core
            .register_http_tunnel(&over_session, None, TunnelProtocol::Http, None)
            .unwrap();
        let (under_tx, mut under_rx) = tokio::sync::mpsc::channel(4);
        core.register_session(
            "127.0.0.1:10".parse().unwrap(),
            "under".into(),
            Some(under_token),
            Some(under.user_id),
            under_tx,
        );
        core.http_routes
            .get(&subdomain)
            .unwrap()
            .members
            .get(&tunnel_id)
            .unwrap()
            .info
            .bytes_in
            .store(
                AgentPlanLimits::BANDWIDTH_LIMIT_BYTES as u64,
                Ordering::Relaxed,
            );

        let gate = AccountGate::new(pool.clone(), Duration::ZERO, Duration::ZERO);
        let closed = disconnect_blocked_sessions(&pool, &core, &gate)
            .await
            .unwrap();
        assert_eq!(closed, 1);
        assert!(matches!(over_rx.try_recv(), Ok(ControlMessage::Shutdown)));
        assert!(under_rx.try_recv().is_err());
        let stored: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log WHERE user_id = $1",
        )
        .bind(over.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, 0, "the recheck must not need a flushed row");
        over.cleanup().await;
        under.cleanup().await;
    }

    #[tokio::test]
    async fn hobby_registration_is_not_rewritten_to_agent_codes() {
        let pool = pool().await;
        let core = idle_core();
        let fixture = Fixture::insert(&pool, "human", "free", false, "active").await;
        let denial =
            evaluate_plan_registration(&pool, fixture.user_id, "tcp", Some("myapp"), &core)
                .await
                .unwrap();
        assert!(denial.is_none());
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn sweep_deactivates_only_expired_unclaimed_agents_and_is_idempotent() {
        let pool = pool().await;
        let expired = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let fresh = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let claimed = Fixture::insert(&pool, "agent", "agent", true, "active").await;
        let human = Fixture::insert(&pool, "human", "free", false, "active").await;
        let mixed = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let tokenless = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let past = Utc::now() - chrono::Duration::seconds(5);
        let future = Utc::now() + chrono::Duration::days(3);
        add_token(&pool, expired.user_id, Some(past)).await;
        add_token(&pool, fresh.user_id, Some(future)).await;
        add_token(&pool, claimed.user_id, Some(past)).await;
        add_token(&pool, human.user_id, Some(past)).await;
        add_token(&pool, mixed.user_id, Some(past)).await;
        add_token(&pool, mixed.user_id, None).await;

        let changed = deactivate_expired_unclaimed_agents(&pool).await.unwrap();
        assert!(changed >= 1);

        let stamp = deactivated_at(&pool, expired.user_id).await;
        assert!(stamp.is_some());
        assert!(deactivated_at(&pool, fresh.user_id).await.is_none());
        assert!(deactivated_at(&pool, claimed.user_id).await.is_none());
        assert!(deactivated_at(&pool, human.user_id).await.is_none());
        assert!(deactivated_at(&pool, mixed.user_id).await.is_none());
        assert!(deactivated_at(&pool, tokenless.user_id).await.is_none());

        let user_row: (String,) = sqlx::query_as("SELECT status FROM users WHERE id = $1")
            .bind(expired.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(user_row.0, "active");
        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens WHERE user_id = $1")
            .bind(expired.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tokens, 1);

        deactivate_expired_unclaimed_agents(&pool).await.unwrap();
        assert_eq!(deactivated_at(&pool, expired.user_id).await, stamp);

        let email = format!("agent-{}", expired.user_id);
        let filters = crate::db::AdminUserFilters {
            kind: Some("agent"),
            search: Some(&email),
            ..crate::db::AdminUserFilters::default()
        };
        let listed =
            crate::db::list_admin_users(&pool, 10, 0, filters, crate::db::AdminUserSort::Created)
                .await
                .unwrap();
        let row = listed
            .iter()
            .find(|user| user.id == expired.user_id)
            .unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.source.as_deref(), Some("api"));
        assert!(row.claimed_at.is_none());
        assert!(row.deactivated_at.is_some());
        let humans = crate::db::count_admin_users(
            &pool,
            crate::db::AdminUserFilters {
                kind: Some("human"),
                search: Some(&email),
                ..crate::db::AdminUserFilters::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(humans, 0);
        expired.cleanup().await;
        fresh.cleanup().await;
        claimed.cleanup().await;
        human.cleanup().await;
        mixed.cleanup().await;
        tokenless.cleanup().await;
    }

    async fn deactivated_at(pool: &PgPool, user_id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT deactivated_at FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }
}
