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
}

fn default_dashboard_port() -> u16 {
    4040
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSection {
    /// Filesystem path for the embedded database (e.g. SQLite file)
    pub path: String,
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

        toml::from_str(&raw).map_err(|e| Error::Config(format!("invalid config TOML: {e}")))
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
            },
            database: DatabaseSection {
                path: ":memory:".to_string(),
            },
            logging: LoggingSection {
                level: "info".to_string(),
                format: "pretty".to_string(),
                audit_log_path: None,
            },
            limits: LimitsSection {
                max_tunnels_per_session: 10,
                max_connections_per_tunnel: 100,
                rate_limit_rps: 100,
                ip_rate_limit_rps: 1000,
                request_body_max_bytes: 10 * 1024 * 1024,
                tcp_port_range: [20000, 20099],
            },
        }
    }
}
