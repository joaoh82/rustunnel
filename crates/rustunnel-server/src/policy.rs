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
//! month bytes for [`BANDWIDTH_CACHE_TTL`]. Registration and token auth read
//! Postgres directly. A background pass every [`SESSION_RECHECK_INTERVAL`]
//! re-reads tokens and users and closes sessions that are no longer allowed,
//! so a ban or deactivation lands within about a minute even when the
//! per-request cache is warm. A database error on that pass or on a proxied
//! request leaves the session up (a blip must not drop every tunnel).
//! Registration fails closed: a plan lookup error refuses the tunnel.
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

use crate::billing::month_bytes_proxied;
use crate::core::{ControlMessage, TunnelCore};
use crate::db::models::{AgentPlanLimits, Token};
use crate::error::Result;

/// Per-request account snapshot. A ban still closes the session on the
/// shorter [`SESSION_RECHECK_INTERVAL`] pass, which does not use this cache.
pub const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(30);
/// Month-byte total used for the hard cap. Short enough that crossing the
/// cap blocks the next requests without a query on every one.
pub const BANDWIDTH_CACHE_TTL: Duration = Duration::from_secs(15);
/// How often live sessions are checked for a ban, suspension, deactivation,
/// or an expired token.
pub const SESSION_RECHECK_INTERVAL: Duration = Duration::from_secs(20);
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
}

struct CachedView {
    view: AccountView,
    at: Instant,
}

struct CachedBytes {
    bytes: i64,
    at: Instant,
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
    pub async fn view(&self, user_id: Uuid) -> Result<Option<AccountView>> {
        if let Some(hit) = self.fresh_view(user_id) {
            return Ok(Some(hit));
        }
        let loaded = load_account(&self.pool, user_id).await?;
        if self.account_ttl.is_zero() {
            return Ok(loaded);
        }
        if let Some(ref view) = loaded {
            self.accounts.insert(
                user_id,
                CachedView {
                    view: view.clone(),
                    at: Instant::now(),
                },
            );
        }
        Ok(loaded)
    }

    /// UTC-month `tunnel_log.bytes_proxied` for `user_id`.
    pub async fn month_bytes(&self, user_id: Uuid) -> Result<i64> {
        if let Some(entry) = self.bandwidth.get(&user_id) {
            if fresh(entry.at, self.bandwidth_ttl) {
                return Ok(entry.bytes);
            }
        }
        let bytes = month_bytes_proxied(&self.pool, user_id).await?;
        if !self.bandwidth_ttl.is_zero() {
            self.bandwidth.insert(
                user_id,
                CachedBytes {
                    bytes,
                    at: Instant::now(),
                },
            );
        }
        Ok(bytes)
    }

    fn fresh_view(&self, user_id: Uuid) -> Option<AccountView> {
        let entry = self.accounts.get(&user_id)?;
        if fresh(entry.at, self.account_ttl) {
            Some(entry.view.clone())
        } else {
            None
        }
    }
}

fn fresh(at: Instant, ttl: Duration) -> bool {
    !ttl.is_zero() && at.elapsed() < ttl
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
        month_bytes_proxied(pool, user_id).await?
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

async fn load_account(pool: &PgPool, user_id: Uuid) -> Result<Option<AccountView>> {
    let row: Option<AccountRow> = sqlx::query_as(
        "SELECT u.status, u.kind, u.claimed_at, u.deactivated_at, \
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
         ) p ON TRUE \
         WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| {
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
    }))
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
/// The returned path drops only [`WARNING_ACK_PARAM`] and stays on this host.
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

fn safe_path(path: &str) -> &str {
    if path.starts_with('/') && !path.starts_with("//") {
        path
    } else {
        "/"
    }
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
    status: String,
    expires_at: Option<DateTime<Utc>>,
    unlimited: bool,
    user_id: Option<Uuid>,
}

/// Close sessions whose token or user is no longer allowed.
///
/// Reads Postgres on every call (not the request cache). Sends
/// [`ControlMessage::Shutdown`]; the session task drops its tunnels.
pub async fn disconnect_blocked_sessions(pool: &PgPool, core: &TunnelCore) -> Result<usize> {
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
    let mut closed = 0usize;
    for (session_id, token_id) in snapshot {
        let blocked = match session_blocked(pool, &token_id).await {
            Ok(blocked) => blocked,
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "session access recheck failed");
                false
            }
        };
        if !blocked {
            continue;
        }
        let sent = core.sessions.get(&session_id).is_some_and(|session| {
            session
                .control_tx
                .try_send(ControlMessage::Shutdown)
                .is_ok()
        });
        if sent {
            closed += 1;
            tracing::info!(%session_id, "closing session after account recheck");
        }
    }
    Ok(closed)
}

async fn session_blocked(pool: &PgPool, token_id: &str) -> Result<bool> {
    let token: Option<LiveToken> =
        sqlx::query_as("SELECT status, expires_at, unlimited, user_id FROM tokens WHERE id = $1")
            .bind(token_id)
            .fetch_optional(pool)
            .await?;
    let Some(token) = token else {
        return Ok(true);
    };
    if token.status != "active" {
        return Ok(true);
    }
    if !token.unlimited {
        if let Some(exp) = token.expires_at {
            if Utc::now() > exp {
                return Ok(true);
            }
        }
    }
    let Some(user_id) = token.user_id else {
        return Ok(false);
    };
    match load_account(pool, user_id).await? {
        Some(view) => Ok(account_block(&view).is_some()),
        None => Ok(true),
    }
}

/// Sweep plus session recheck. Runs until the task is aborted on shutdown.
/// The sweep's first tick is immediate, so a restart deactivates before the
/// full interval.
pub async fn run_account_maintenance(pool: PgPool, core: std::sync::Arc<TunnelCore>) {
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
                if let Err(e) = disconnect_blocked_sessions(&pool, &core).await {
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
        assert!(cap.message.contains("262144000"));
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
        let fixture = Fixture::insert(&pool, "agent", "agent", false, "active").await;
        let none = evaluate_plan_registration(&pool, fixture.user_id, "http", None)
            .await
            .unwrap();
        assert!(none.is_none());
        let tcp = evaluate_plan_registration(&pool, fixture.user_id, "tcp", Some("name"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tcp.code, code::PROTOCOL_NOT_ALLOWED);
        let sub = evaluate_plan_registration(&pool, fixture.user_id, "http", Some("myapp"))
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
        let second = evaluate_plan_registration(&pool, fixture.user_id, "https", None)
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
        let capped = evaluate_plan_registration(&pool, fixture.user_id, "http", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(capped.code, code::BANDWIDTH_CAP_REACHED);
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn hobby_registration_is_not_rewritten_to_agent_codes() {
        let pool = pool().await;
        let fixture = Fixture::insert(&pool, "human", "free", false, "active").await;
        let denial = evaluate_plan_registration(&pool, fixture.user_id, "tcp", Some("myapp"))
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
