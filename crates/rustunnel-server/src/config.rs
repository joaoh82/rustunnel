use serde::Deserialize;
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub tls: TlsSection,
    pub auth: AuthSection,
    pub database: DatabaseSection,
    pub logging: LoggingSection,
    pub limits: LimitsSection,
    /// Region identity for this server instance.
    #[serde(default)]
    pub region: RegionSection,
    /// P2P direct connection settings.
    #[serde(default)]
    pub p2p: P2pSection,
    /// Test-only knobs. Used by the integration suite to simulate
    /// older edges (Phase 6 cross-version matrix). **Production
    /// deployments must leave this section out.**
    #[serde(default)]
    pub testing: TestingSection,
    /// Load-balancing kill switch (TUNNEL-7, Phase 6 of the plan).
    /// When `false` (default), grouped registrations fall through and behave
    /// as solo tunnels exactly like today — protocol fields are accepted but
    /// not honoured. Flip per region during the EU → US → AP rollout.
    #[serde(default)]
    pub load_balancing: LoadBalancingSection,
    /// platform-api base URL and shared secret for Hobby-limit reports.
    /// Absent on self-hosted configs: reporting stays off.
    #[serde(default)]
    pub platform: PlatformSection,
}

/// Where the edge reports Hobby-limit hits so platform-api can dedupe and
/// emit `payg_nudge_shown`. Both fields empty (the default) disables the call.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlatformSection {
    /// Base URL of platform-api, for example `https://api.rustunnel.com`.
    #[serde(default)]
    pub api_url: String,
    /// Bearer token platform-api expects on `POST /internal/hobby-limit`.
    #[serde(default)]
    pub internal_token: String,
}

impl PlatformSection {
    /// True only when a report would actually be sent.
    pub fn reporting_enabled(&self) -> bool {
        !self.api_url.trim().is_empty() && !self.internal_token.trim().is_empty()
    }

    /// `POST` target. Trailing slashes on `api_url` are stripped.
    pub fn hobby_limit_url(&self) -> String {
        format!(
            "{}/internal/hobby-limit",
            self.api_url.trim().trim_end_matches('/')
        )
    }
}

fn apply_platform_env_overrides(platform: &mut PlatformSection) {
    if let Ok(url) = std::env::var("RUSTUNNEL_PLATFORM_API_URL") {
        if !url.trim().is_empty() {
            platform.api_url = url;
        }
    }
    if let Ok(token) = std::env::var("RUSTUNNEL_PLATFORM_INTERNAL_TOKEN") {
        if !token.trim().is_empty() {
            platform.internal_token = token;
        }
    }
}

/// Test-only knobs. Adding a `[testing]` section to `server.toml` is a
/// supported way for the integration suite to simulate older edges so we
/// can verify the v0.7.0 client's TUNNEL-7 Phase 6 fall-through behaviour
/// (skip group fields, skip probe loop) against an old `server_version`.
/// Production configs **must not** include this section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TestingSection {
    /// Override the `server_version` advertised in the `AuthOk` frame.
    /// `None` (the default) → use `CARGO_PKG_VERSION`. Set to a string
    /// like `"0.5.0"` in tests to make the server pretend to be an older
    /// edge.
    #[serde(default)]
    pub override_server_version: Option<String>,
}

/// Per-region kill switch and tunables for group-based load balancing.
///
/// The default (`enabled = false`) is intentional — Phase 0 / Phase 1 ship
/// the protocol and routing-table refactor without changing any user-visible
/// behaviour. Phase 2/3 of the plan will start honouring this flag.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LoadBalancingSection {
    /// Master switch. `false` → group/group_key_hash are accepted on
    /// `RegisterTunnel` but ignored; `true` → registrations with the same
    /// `(subdomain, group_key_hash)` form one pool with random dispatch.
    #[serde(default)]
    pub enabled: bool,

    /// Optional webhook URL for "0 healthy members" alerts (TUNNEL-8 Phase 5).
    /// When set, the server POSTs a JSON payload to this URL whenever a
    /// load-balancing group transitions to having zero healthy members.
    /// Debounced — fires once per transition, not per frame. Empty / unset
    /// disables the alert. Best-effort delivery (5s timeout, no retry).
    ///
    /// Example payload:
    /// ```json
    /// {
    ///   "event": "group_zero_healthy",
    ///   "region_id": "eu",
    ///   "protocol": "http",
    ///   "label": "pool",
    ///   "group_name": "web",
    ///   "key_hash_short": "deadbeef",
    ///   "member_count": 2,
    ///   "at": "2026-05-03T18:24:55+00:00"
    /// }
    /// ```
    #[serde(default)]
    pub alert_webhook_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct P2pSection {
    /// Master switch for P2P tunnels. When false, P2P registration and
    /// P2pConnect requests are rejected with a clear error. Direct mode is
    /// gated by `direct_enabled` below; this flag gates P2P entirely
    /// (relay + direct). Defaults to false for safe rollout.
    #[serde(default)]
    pub enabled: bool,
    /// Enable P2P direct mode (NAT hole punching). When false, all P2P
    /// connections use server relay only. Only takes effect when `enabled`
    /// is also true.
    #[serde(default)]
    pub direct_enabled: bool,
    /// STUN servers for NAT type detection (client-side configuration hint).
    #[serde(default = "default_stun_servers")]
    pub stun_servers: Vec<String>,
    /// Hole punch timeout in milliseconds.
    #[serde(default = "default_punch_timeout_ms")]
    pub punch_timeout_ms: u32,
}

fn default_stun_servers() -> Vec<String> {
    vec![
        "stun.l.google.com:19302".into(),
        "stun1.l.google.com:19302".into(),
    ]
}

fn default_punch_timeout_ms() -> u32 {
    5000
}

impl Default for P2pSection {
    fn default() -> Self {
        Self {
            enabled: false,
            direct_enabled: false,
            stun_servers: default_stun_servers(),
            punch_timeout_ms: default_punch_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    /// Primary domain used to build tunnel public URLs (e.g. "tunnel.example.com")
    pub domain: String,
    /// HTTP port for incoming tunnel traffic
    pub http_port: u16,
    /// HTTPS / TLS port for incoming tunnel traffic
    pub https_port: u16,
    /// Port the control-plane WebSocket listens on
    pub control_port: u16,
    /// Port the dashboard HTTP API listens on (default 4040)
    #[serde(default = "default_dashboard_port")]
    pub dashboard_port: u16,
    /// Allowed CORS origin for the external dashboard (e.g. "https://dashboard.rustunnel.com")
    #[serde(default = "default_dashboard_origin")]
    pub dashboard_origin: String,
    /// Plain-HTTP (port 80) behaviour: `"proxy"` forwards requests whose
    /// subdomain resolves to a tunnel (ngrok parity — required for signed
    /// webhooks configured with an `http://` URL) and redirects the rest;
    /// `"redirect"` (default) 308-redirects everything to HTTPS.
    #[serde(default)]
    pub plain_http_mode: crate::edge::PlainHttpMode,
}

fn default_dashboard_port() -> u16 {
    4040
}

fn default_dashboard_origin() -> String {
    "http://localhost:3000".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsSection {
    /// Path to the TLS certificate file (PEM)
    pub cert_path: String,
    /// Path to the TLS private-key file (PEM)
    pub key_path: String,

    // ── ACME / Let's Encrypt ─────────────────────────────────────────────────
    /// Enable automatic certificate issuance and renewal via ACME.
    #[serde(default)]
    pub acme_enabled: bool,

    /// Contact email registered with the ACME CA.
    #[serde(default)]
    pub acme_email: String,

    /// Use Let's Encrypt staging CA (for testing — avoids rate limits).
    #[serde(default)]
    pub acme_staging: bool,

    /// Directory used to persist the ACME account key and credentials.
    #[serde(default = "default_acme_account_dir")]
    pub acme_account_dir: String,

    /// Cloudflare API token with DNS:Edit permission.
    /// Prefer supplying via `CLOUDFLARE_API_TOKEN` environment variable.
    #[serde(default)]
    pub cloudflare_api_token: String,

    /// Cloudflare Zone ID for the domain.
    /// Prefer supplying via `CLOUDFLARE_ZONE_ID` environment variable.
    #[serde(default)]
    pub cloudflare_zone_id: String,
}

fn default_acme_account_dir() -> String {
    "/var/lib/rustunnel".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthSection {
    /// Token used for administrative operations
    pub admin_token: String,
    /// When true every client must present a valid auth token
    pub require_auth: bool,
    /// Failed authentication attempts allowed per source IP per minute before
    /// further attempts are rejected outright (0 = disabled). Applies to the
    /// control-plane `Auth` handshake and the dashboard API bearer check.
    #[serde(default = "default_max_failed_auth_per_minute")]
    pub max_failed_auth_per_minute: u32,
}

fn default_max_failed_auth_per_minute() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSection {
    /// PostgreSQL connection URL for shared data (tokens, tunnel_log).
    /// e.g. "postgresql://rustunnel:password@10.0.0.3:5432/rustunnel"
    pub url: String,

    /// Local SQLite file path for per-region captured request data.
    #[serde(default = "default_captured_db_path")]
    pub captured_path: String,
}

fn default_captured_db_path() -> String {
    "/var/lib/rustunnel/captured.db".to_string()
}

/// Identity of this server instance within the multi-region fleet.
///
/// Optional — omit the `[region]` section entirely for single-server deployments.
/// When present the `id` is stamped on every `tunnel_log` row and returned in
/// `GET /api/status` and `GET /api/regions` so the dashboard can route
/// captured-request queries back to the correct regional server.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionSection {
    /// Short identifier matching a row in the `regions` table (e.g. `"eu"`).
    #[serde(default = "default_region_id")]
    pub id: String,
    /// Human-readable region name (e.g. `"Europe"`).
    #[serde(default)]
    pub name: String,
    /// Data-centre location (e.g. `"Falkenstein, DE"`).
    #[serde(default)]
    pub location: String,
}

fn default_region_id() -> String {
    "default".to_string()
}

impl Default for RegionSection {
    fn default() -> Self {
        Self {
            id: default_region_id(),
            name: String::new(),
            location: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingSection {
    /// Log verbosity level: "trace" | "debug" | "info" | "warn" | "error"
    pub level: String,
    /// Output format: "json" | "pretty"
    pub format: String,
    /// Optional path for the audit log file (JSON-lines).  Omit to disable.
    #[serde(default)]
    pub audit_log_path: Option<String>,

    /// Sentry DSN for error tracking and performance monitoring.
    /// Prefer supplying via `SENTRY_DSN` environment variable.
    /// Omit or leave empty to disable Sentry.
    #[serde(default)]
    pub sentry_dsn: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsSection {
    /// Maximum number of tunnels a single authenticated session may hold
    pub max_tunnels_per_session: usize,
    /// Maximum number of concurrent proxied connections per tunnel
    pub max_connections_per_tunnel: usize,
    /// Per-tunnel request rate limit in requests-per-second
    pub rate_limit_rps: u32,
    /// Per-source-IP request rate limit in requests-per-second (0 = disabled)
    #[serde(default = "default_ip_rate_limit_rps")]
    pub ip_rate_limit_rps: u32,
    /// Maximum size of a proxied request body in bytes
    pub request_body_max_bytes: usize,
    /// Inclusive [low, high] port range reserved for TCP tunnels
    pub tcp_port_range: [u16; 2],
    /// Inclusive [low, high] port range reserved for UDP tunnels.
    /// Set to [0, 0] to disable UDP tunnels.
    #[serde(default = "default_udp_port_range")]
    pub udp_port_range: [u16; 2],
}

fn default_udp_port_range() -> [u16; 2] {
    [0, 0]
}

fn default_ip_rate_limit_rps() -> u32 {
    100
}

impl ServerConfig {
    /// Load configuration from a TOML file at `path`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::Config(format!(
                "cannot read config file {}: {e}",
                path.as_ref().display()
            ))
        })?;

        let mut cfg: Self =
            toml::from_str(&raw).map_err(|e| Error::Config(format!("invalid config TOML: {e}")))?;
        apply_platform_env_overrides(&mut cfg.platform);
        Ok(cfg)
    }
}

// ── defaults used in tests ────────────────────────────────────────────────────

#[cfg(test)]
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSection {
                domain: "localhost".to_string(),
                http_port: 8080,
                https_port: 8443,
                control_port: 9000,
                dashboard_port: 4040,
                dashboard_origin: "http://localhost:3000".to_string(),
                plain_http_mode: crate::edge::PlainHttpMode::default(),
            },
            tls: TlsSection {
                cert_path: "cert.pem".to_string(),
                key_path: "key.pem".to_string(),
                acme_enabled: false,
                acme_email: String::new(),
                acme_staging: true,
                acme_account_dir: "/tmp/rustunnel-test".to_string(),
                cloudflare_api_token: String::new(),
                cloudflare_zone_id: String::new(),
            },
            auth: AuthSection {
                admin_token: "test-admin-token".to_string(),
                require_auth: false,
                max_failed_auth_per_minute: 10,
            },
            database: DatabaseSection {
                url: std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
                    "postgres://rustunnel:test@localhost:5432/rustunnel_test".to_string()
                }),
                captured_path: ":memory:".to_string(),
            },
            logging: LoggingSection {
                level: "info".to_string(),
                format: "pretty".to_string(),
                audit_log_path: None,
                sentry_dsn: None,
            },
            limits: LimitsSection {
                max_tunnels_per_session: 10,
                max_connections_per_tunnel: 100,
                rate_limit_rps: 100,
                ip_rate_limit_rps: 1000,
                request_body_max_bytes: 10 * 1024 * 1024,
                tcp_port_range: [20000, 20099],
                udp_port_range: [0, 0],
            },
            region: RegionSection {
                id: "test".to_string(),
                name: "Test Region".to_string(),
                location: "localhost".to_string(),
            },
            p2p: P2pSection::default(),
            load_balancing: LoadBalancingSection::default(),
            testing: TestingSection::default(),
            platform: PlatformSection::default(),
        }
    }
}
