//! Hobby-plan limit copy and the report the edge sends to platform-api.
//!
//! Concurrent tunnels and custom subdomains are rejected here, on the edge,
//! inside `control/session.rs`. Released CLIs (v0.8.5 and earlier) print
//! `TunnelError.message` as `error: tunnel error: {message}` and ignore any
//! new frame field, so the upgrade sentence has to be that string.
//!
//! Bandwidth is not a hard reject. `plans.bandwidth_limit_gb` was never read
//! before this module. When month-to-date `tunnel_log.bytes_proxied` is at
//! least 80% of that cap, registration still succeeds and
//! `TunnelRegistered.notice` carries a warning the new CLI prints.
//!
//! # platform-api contract
//!
//! When `[platform] api_url` and `[platform] internal_token` are both set
//! (or the `RUSTUNNEL_PLATFORM_API_URL` / `RUSTUNNEL_PLATFORM_INTERNAL_TOKEN`
//! env vars), every hit is reported with:
//!
//! ```text
//! POST {api_url}/internal/hobby-limit
//! Authorization: Bearer {internal_token}
//! Content-Type: application/json
//!
//! {
//!   "user_id": "<uuid>",
//!   "reason": "tunnel_limit" | "custom_subdomain" | "bandwidth",
//!   "surface": "cli" | "mcp",
//!   "region_id": "eu",
//!   "upgrade_url": "https://rustunnel.com/dashboard/billing?upgrade=1&..."
//! }
//! ```
//!
//! platform-api should dedupe (same user + reason) and emit PostHog
//! `payg_nudge_shown` with those `reason` and `surface` properties. Any
//! failure is logged and ignored: the CLI message does not depend on this
//! call. `reason=custom_subdomain` matches the existing dashboard nudge;
//! the upgrade URL uses `utm_content=subdomain`, which is the public link
//! parameter, not the event name.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::PlatformSection;
use crate::error::Result;

/// Binary gigabyte (1024^3). Same unit billing and platform-api use for
/// Hobby bandwidth caps.
const BYTES_PER_GB: i64 = 1_073_741_824;
const BANDWIDTH_WARN_PERCENT: u128 = 80;

const UPGRADE_ORIGIN: &str = "https://rustunnel.com/dashboard/billing?upgrade=1";

/// Who is asking. Released CLIs omit the field and are treated as `cli`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Cli,
    Mcp,
}

impl Surface {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw
            .map(str::trim)
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("mcp") => Surface::Mcp,
            _ => Surface::Cli,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Mcp => "mcp",
        }
    }
}

/// A Hobby limit the edge is prepared to talk about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HobbyReason {
    TunnelLimit { limit: i32 },
    CustomSubdomain,
    Bandwidth { percent: u32, limit_gb: i32 },
}

impl HobbyReason {
    /// PostHog `payg_nudge_shown` reason. `custom_subdomain` matches the
    /// existing dashboard event so the funnel does not split.
    pub fn code(&self) -> &'static str {
        match self {
            HobbyReason::TunnelLimit { .. } => "tunnel_limit",
            HobbyReason::CustomSubdomain => "custom_subdomain",
            HobbyReason::Bandwidth { .. } => "bandwidth",
        }
    }

    /// `utm_content` on the public upgrade URL.
    pub fn utm_content(&self) -> &'static str {
        match self {
            HobbyReason::TunnelLimit { .. } => "tunnel_limit",
            HobbyReason::CustomSubdomain => "subdomain",
            HobbyReason::Bandwidth { .. } => "bandwidth",
        }
    }

    pub fn upgrade_url(&self, surface: Surface) -> String {
        format!(
            "{UPGRADE_ORIGIN}&utm_source={}&utm_campaign=hobby_limit&utm_content={}",
            surface.as_str(),
            self.utm_content()
        )
    }

    /// Full text a released CLI will print inside its existing error line.
    pub fn message(&self, surface: Surface) -> String {
        let upgrade = self.upgrade_url(surface);
        match self {
            HobbyReason::TunnelLimit { limit } => format!(
                "Hobby plan limit: {limit} concurrent tunnels. Pay-as-you-go removes the cap ($3/mo min).\nUpgrade: {upgrade}"
            ),
            HobbyReason::CustomSubdomain => format!(
                "Hobby plan limit: custom subdomains need Pay-as-you-go ($3/mo min, $0.10/GB).\nUpgrade: {upgrade}"
            ),
            HobbyReason::Bandwidth { percent, limit_gb } => {
                // Matches platform-api `usage_cli_notice`. At 100% and above
                // this is still a notice: bandwidth is not a hard stop.
                let body = if *percent >= 100 {
                    format!(
                        "Hobby plan: {limit_gb} GB/mo included bandwidth used. Pay-as-you-go is $0.10/GB ($3/mo min)."
                    )
                } else {
                    format!(
                        "Hobby plan: {percent}% of {limit_gb} GB/mo used. Pay-as-you-go is $0.10/GB ($3/mo min)."
                    )
                };
                format!("{body}\nUpgrade: {upgrade}")
            }
        }
    }
}

/// Outcome of the plan check for one `RegisterTunnel`.
#[derive(Debug, Default)]
pub struct HobbyGate {
    /// Registration must be rejected with this reason's message.
    pub block: Option<HobbyReason>,
    /// Registration proceeds. New CLIs print this as `TunnelRegistered.notice`.
    pub notice: Option<HobbyReason>,
}

/// Pure decision for the two hard Hobby gates. Tunnel count is checked
/// first, matching the historical order in `control/session.rs`.
pub fn blocking_limit(
    max_tunnels: Option<i32>,
    open_tunnels: i64,
    allow_custom_subdomains: bool,
    requested_subdomain: Option<&str>,
) -> Option<HobbyReason> {
    if let Some(limit) = max_tunnels {
        if open_tunnels >= i64::from(limit) {
            return Some(HobbyReason::TunnelLimit { limit });
        }
    }
    if !allow_custom_subdomains && requested_subdomain.map(|s| !s.is_empty()).unwrap_or(false) {
        return Some(HobbyReason::CustomSubdomain);
    }
    None
}

/// `Some` when `used_bytes` is at least 80% of `limit_gb` GiB (1024^3).
/// Percent is integer floor division.
pub fn bandwidth_warning(used_bytes: i64, limit_gb: Option<i32>) -> Option<HobbyReason> {
    let limit_gb = limit_gb.filter(|gb| *gb > 0)?;
    if used_bytes <= 0 {
        return None;
    }
    let limit_bytes = i64::from(limit_gb).saturating_mul(BYTES_PER_GB);
    if limit_bytes <= 0 {
        return None;
    }
    let percent = (used_bytes as u128).saturating_mul(100) / (limit_bytes as u128);
    if percent < BANDWIDTH_WARN_PERCENT {
        return None;
    }
    Some(HobbyReason::Bandwidth {
        percent: u32::try_from(percent).unwrap_or(u32::MAX),
        limit_gb,
    })
}

/// Apply Hobby gates only when `billing_model` is exactly `"free"`.
///
/// A future metered or subscription plan may carry numeric caps. Those caps
/// must not produce a Hobby nudge or a bandwidth notice.
fn gate_for_plan(
    billing_model: &str,
    max_tunnels: Option<i32>,
    open_tunnels: i64,
    allow_custom_subdomains: bool,
    requested_subdomain: Option<&str>,
    used_bytes: i64,
    bandwidth_limit_gb: Option<i32>,
) -> HobbyGate {
    if billing_model != "free" {
        return HobbyGate::default();
    }
    let block = blocking_limit(
        max_tunnels,
        open_tunnels,
        allow_custom_subdomains,
        requested_subdomain,
    );
    if block.is_some() {
        return HobbyGate {
            block,
            notice: None,
        };
    }
    HobbyGate {
        block: None,
        notice: bandwidth_warning(used_bytes, bandwidth_limit_gb),
    }
}

/// Load the active plan and decide the block / bandwidth notice.
///
/// No active subscription means no Hobby gate. That is the same branch the
/// edge has always used: direct, admin, and legacy tokens, and any user
/// row without `subscriptions.status = 'active'`, are not capped here.
///
/// Only `plans.billing_model = 'free'` is gated. Anything else returns
/// immediately, before the open-tunnel count and the byte sum.
pub async fn evaluate_hobby_limits(
    pool: &PgPool,
    user_id: Uuid,
    requested_subdomain: Option<&str>,
) -> Result<HobbyGate> {
    let plan = sqlx::query_as::<_, (Option<i32>, bool, Option<i32>, String)>(
        "SELECT p.max_tunnels, p.allow_custom_subdomains, p.bandwidth_limit_gb, \
                p.billing_model \
         FROM subscriptions s \
         JOIN plans p ON p.id = s.plan_id \
         WHERE s.user_id = $1 AND s.status = 'active' \
         ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    let Some((max_tunnels, allow_custom_subdomains, bandwidth_limit_gb, billing_model)) = plan
    else {
        return Ok(HobbyGate::default());
    };

    if billing_model != "free" {
        return Ok(HobbyGate::default());
    }

    let open_tunnels: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM tunnel_log \
         WHERE user_id = $1 AND unregistered_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    let block = blocking_limit(
        max_tunnels,
        open_tunnels.0,
        allow_custom_subdomains,
        requested_subdomain,
    );
    if block.is_some() {
        return Ok(HobbyGate {
            block,
            notice: None,
        });
    }

    let used_bytes = if bandwidth_limit_gb.is_some() {
        match month_bytes_proxied(pool, user_id).await {
            Ok(used) => used,
            Err(e) => {
                tracing::warn!(error = %e, %user_id, "bandwidth usage lookup failed");
                0
            }
        }
    } else {
        0
    };

    Ok(gate_for_plan(
        &billing_model,
        max_tunnels,
        open_tunnels.0,
        allow_custom_subdomains,
        requested_subdomain,
        used_bytes,
        bandwidth_limit_gb,
    ))
}

/// Bytes recorded on tunnels that closed during the current UTC month.
///
/// `tunnel_log.bytes_proxied` is written when a tunnel unregisters, so
/// in-flight tunnels are not in this sum until they close.
async fn month_bytes_proxied(pool: &PgPool, user_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log \
         WHERE user_id = $1 \
           AND unregistered_at >= date_trunc('month', (now() AT TIME ZONE 'UTC')) AT TIME ZONE 'UTC'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Fire-and-forget report. No-op when platform-api is not configured.
pub fn spawn_hobby_limit_report(
    platform: &PlatformSection,
    region_id: &str,
    user_id: Uuid,
    reason: &HobbyReason,
    surface: Surface,
) {
    if !platform.reporting_enabled() {
        tracing::debug!(
            reason = reason.code(),
            "hobby-limit report skipped; platform api is unset"
        );
        return;
    }
    let platform = platform.clone();
    let region_id = region_id.to_string();
    let reason = reason.clone();
    tokio::spawn(async move {
        if let Err(e) = post_hobby_limit(&platform, &region_id, user_id, &reason, surface).await {
            tracing::warn!(
                error = %e,
                reason = reason.code(),
                "hobby-limit report to platform-api failed"
            );
        }
    });
}

#[derive(Serialize)]
struct HobbyLimitBody<'a> {
    user_id: Uuid,
    reason: &'a str,
    surface: &'a str,
    region_id: &'a str,
    upgrade_url: String,
}

async fn post_hobby_limit(
    platform: &PlatformSection,
    region_id: &str,
    user_id: Uuid,
    reason: &HobbyReason,
    surface: Surface,
) -> std::result::Result<(), String> {
    let body = HobbyLimitBody {
        user_id,
        reason: reason.code(),
        surface: surface.as_str(),
        region_id,
        upgrade_url: reason.upgrade_url(surface),
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .post(platform.hobby_limit_url())
        .bearer_auth(&platform.internal_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("post: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("platform-api returned {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TUNNEL_CLI: &str = "\
Hobby plan limit: 2 concurrent tunnels. Pay-as-you-go removes the cap ($3/mo min).\n\
Upgrade: https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=tunnel_limit";

    const SUBDOMAIN_CLI: &str = "\
Hobby plan limit: custom subdomains need Pay-as-you-go ($3/mo min, $0.10/GB).\n\
Upgrade: https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=subdomain";

    #[test]
    fn tunnel_limit_copy_matches_the_cli_string() {
        let msg = HobbyReason::TunnelLimit { limit: 2 }.message(Surface::Cli);
        assert_eq!(msg, TUNNEL_CLI);
        assert!(!msg.contains('—'));
        assert!(!msg.contains('–'));
    }

    #[test]
    fn subdomain_copy_matches_the_cli_string() {
        let msg = HobbyReason::CustomSubdomain.message(Surface::Cli);
        assert_eq!(msg, SUBDOMAIN_CLI);
        assert!(!msg.contains('—'));
    }

    #[test]
    fn mcp_surface_swaps_only_the_utm_source() {
        let msg = HobbyReason::TunnelLimit { limit: 2 }.message(Surface::Mcp);
        assert!(msg.contains("utm_source=mcp"));
        assert!(msg.contains("utm_content=tunnel_limit"));
        assert!(!msg.contains("utm_source=cli"));
        let sub = HobbyReason::CustomSubdomain.message(Surface::Mcp);
        assert!(sub.contains("utm_source=mcp"));
        assert!(sub.contains("utm_content=subdomain"));
    }

    #[test]
    fn surface_parse_treats_released_clients_as_cli() {
        assert_eq!(Surface::parse(None), Surface::Cli);
        assert_eq!(Surface::parse(Some("")), Surface::Cli);
        assert_eq!(Surface::parse(Some("cli")), Surface::Cli);
        assert_eq!(Surface::parse(Some(" MCP ")), Surface::Mcp);
    }

    #[test]
    fn blocking_limit_order_matches_the_historical_checks() {
        assert_eq!(
            blocking_limit(Some(2), 2, false, Some("myapp")),
            Some(HobbyReason::TunnelLimit { limit: 2 })
        );
        assert_eq!(
            blocking_limit(Some(2), 1, false, Some("myapp")),
            Some(HobbyReason::CustomSubdomain)
        );
        assert_eq!(blocking_limit(Some(2), 1, false, None), None);
        assert_eq!(blocking_limit(Some(2), 1, false, Some("")), None);
        assert_eq!(blocking_limit(Some(2), 1, true, Some("myapp")), None);
        assert_eq!(blocking_limit(None, 99, true, None), None);
    }

    const BANDWIDTH_URL: &str = "https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=bandwidth";

    fn bandwidth_notice(percent_line: &str) -> String {
        format!("{percent_line}\nUpgrade: {BANDWIDTH_URL}")
    }

    #[test]
    fn bandwidth_warning_starts_at_80_percent_of_a_gib() {
        // Floor division: 80% of 1 GiB is 858_993_460 bytes. One byte less is 79%.
        const UNDER_80: i64 = 858_993_459;
        const AT_80: i64 = 858_993_460;
        const AT_99: i64 = 1_073_741_823;
        const AT_100: i64 = BYTES_PER_GB;

        assert_eq!(bandwidth_warning(UNDER_80, Some(1)), None);
        assert_eq!(
            bandwidth_warning(AT_80, Some(1)),
            Some(HobbyReason::Bandwidth {
                percent: 80,
                limit_gb: 1,
            })
        );
        assert_eq!(bandwidth_warning(0, Some(1)), None);
        assert_eq!(bandwidth_warning(50_000_000_000, None), None);

        let at_80 = bandwidth_warning(AT_80, Some(1))
            .unwrap()
            .message(Surface::Cli);
        assert_eq!(
            at_80,
            bandwidth_notice(
                "Hobby plan: 80% of 1 GB/mo used. Pay-as-you-go is $0.10/GB ($3/mo min)."
            )
        );

        let at_99 = bandwidth_warning(AT_99, Some(1)).unwrap();
        assert_eq!(
            at_99,
            HobbyReason::Bandwidth {
                percent: 99,
                limit_gb: 1,
            }
        );
        assert_eq!(
            at_99.message(Surface::Cli),
            bandwidth_notice(
                "Hobby plan: 99% of 1 GB/mo used. Pay-as-you-go is $0.10/GB ($3/mo min)."
            )
        );

        let at_100 = bandwidth_warning(AT_100, Some(1)).unwrap();
        assert_eq!(
            at_100,
            HobbyReason::Bandwidth {
                percent: 100,
                limit_gb: 1
            }
        );
        let included = bandwidth_notice(
            "Hobby plan: 1 GB/mo included bandwidth used. Pay-as-you-go is $0.10/GB ($3/mo min).",
        );
        assert_eq!(at_100.message(Surface::Cli), included);
        // Past the included amount the sentence stays a notice, not a hard stop.
        let over = bandwidth_warning(AT_100.saturating_mul(2), Some(1))
            .unwrap()
            .message(Surface::Cli);
        assert_eq!(over, included);
        assert!(!over.to_ascii_lowercase().contains("limit reached"));
        assert!(!over.to_ascii_lowercase().contains("stopped"));
    }

    #[test]
    fn non_free_plans_are_not_nudged_even_with_numeric_caps() {
        let over_bytes = BYTES_PER_GB.saturating_mul(3);
        for model in ["metered", "subscription", "FREE", ""] {
            let gate = gate_for_plan(model, Some(2), 5, false, Some("myapp"), over_bytes, Some(1));
            assert_eq!(gate.block, None, "{model}");
            assert_eq!(gate.notice, None, "{model}");
        }

        let free = gate_for_plan(
            "free",
            Some(2),
            5,
            false,
            Some("myapp"),
            over_bytes,
            Some(1),
        );
        assert_eq!(free.block, Some(HobbyReason::TunnelLimit { limit: 2 }));
        assert_eq!(free.notice, None);

        let notice = gate_for_plan("free", Some(2), 0, true, None, 858_993_460, Some(1));
        assert_eq!(notice.block, None);
        assert_eq!(
            notice.notice,
            Some(HobbyReason::Bandwidth {
                percent: 80,
                limit_gb: 1,
            })
        );
    }

    #[tokio::test]
    async fn report_posts_the_platform_api_contract() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<(String, serde_json::Value)>>> = Arc::new(Mutex::new(None));
        let seen_route = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/internal/hobby-limit",
            axum::routing::post({
                move |headers: axum::http::HeaderMap, body: String| {
                    let seen_route = Arc::clone(&seen_route);
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let json: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                        *seen_route.lock().expect("seen lock") = Some((auth, json));
                        axum::http::StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let platform = PlatformSection {
            api_url: format!("http://{addr}"),
            internal_token: "secret-token".into(),
        };
        let user_id = Uuid::nil();
        let reason = HobbyReason::TunnelLimit { limit: 2 };
        let mut posted = None;
        for _ in 0..20 {
            match post_hobby_limit(&platform, "eu", user_id, &reason, Surface::Cli).await {
                Ok(()) => {
                    posted = Some(());
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        posted.expect("report posted");

        let (auth, json) = seen.lock().expect("seen lock").clone().expect("captured");
        assert_eq!(auth, "Bearer secret-token");
        assert_eq!(json["user_id"], user_id.to_string());
        assert_eq!(json["reason"], "tunnel_limit");
        assert_eq!(json["surface"], "cli");
        assert_eq!(json["region_id"], "eu");
        assert_eq!(
            json["upgrade_url"],
            "https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=tunnel_limit"
        );
    }
}
