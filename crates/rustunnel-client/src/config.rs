//! Client configuration.
//!
//! Loaded from `~/.rustunnel/config.yml` (or a path given by `--config`).
//! CLI flags always override config-file values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

// ── top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientConfig {
    /// Tunnel server address, e.g. `tunnel.example.com:9000`.
    #[serde(default)]
    pub server: String,

    /// Auth token sent in the `Auth` control frame.
    pub auth_token: Option<String>,

    /// Skip TLS certificate verification (for local development only).
    #[serde(default)]
    pub insecure: bool,

    /// Region preference: `"auto"` (probe & pick nearest), or an explicit ID
    /// like `"eu"`, `"us"`, `"ap"`. Omit for single-server / self-hosted setups.
    pub region: Option<String>,

    /// Named tunnel definitions (used by `rustunnel start`).
    #[serde(default)]
    pub tunnels: HashMap<String, TunnelDef>,
}

// ── tunnel definition ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TunnelDef {
    /// Protocol: `"http"` or `"tcp"`.
    pub proto: String,
    /// Local port to forward to.
    pub local_port: u16,
    /// Local hostname to forward to (default: `"localhost"`).
    #[serde(default = "default_local_host")]
    pub local_host: String,
    /// Requested HTTP subdomain (HTTP tunnels only).
    pub subdomain: Option<String>,
}

fn default_local_host() -> String {
    "localhost".to_string()
}

impl TunnelDef {
    /// Build a `TunnelDef` from inline CLI arguments.
    pub fn from_cli(proto: &str, port: u16, local_host: &str, subdomain: Option<String>) -> Self {
        Self {
            proto: proto.to_string(),
            local_port: port,
            local_host: local_host.to_string(),
            subdomain,
        }
    }
}

// ── loading ───────────────────────────────────────────────────────────────────

impl ClientConfig {
    /// Load from the default location (`~/.rustunnel/config.yml`).
    /// Returns a default empty config if the file does not exist.
    pub fn load_default() -> Result<Self> {
        let path = default_config_path()?;
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Load from an explicit file path.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::Config(format!(
                "cannot read config file {}: {e}",
                path.as_ref().display()
            ))
        })?;
        serde_yaml::from_str(&raw).map_err(|e| Error::Config(format!("invalid config YAML: {e}")))
    }

    /// Validate that required fields are present.
    pub fn validate(&self) -> Result<()> {
        if self.server.is_empty() {
            return Err(Error::Config(
                "server address is required (use --server or set `server` in config)".into(),
            ));
        }
        Ok(())
    }
}

fn default_config_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
    Ok(home.join(".rustunnel").join("config.yml"))
}
