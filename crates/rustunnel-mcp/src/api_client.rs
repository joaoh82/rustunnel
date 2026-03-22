//! HTTP client wrapper for the rustunnel dashboard REST API.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    base_url: String,
}

/// Tunnel summary returned by GET /api/tunnels.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TunnelSummary {
    pub tunnel_id: String,
    pub protocol: String,
    pub label: String,
    pub public_url: String,
    pub connected_since: String,
    pub request_count: u64,
    pub client_addr: String,
}

impl ApiClient {
    /// Create a new client targeting `base_url` (e.g. `http://localhost:4041`).
    ///
    /// `insecure` disables TLS certificate verification, useful when the
    /// dashboard is behind a self-signed cert in local dev.
    pub fn new(base_url: impl Into<String>, insecure: bool) -> Self {
        let client = Client::builder()
            .danger_accept_invalid_certs(insecure)
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.into(),
        }
    }

    pub async fn list_tunnels(&self, token: &str) -> reqwest::Result<Vec<TunnelSummary>> {
        self.client
            .get(format!("{}/api/tunnels", self.base_url))
            .bearer_auth(token)
            .send()
            .await?
            .json()
            .await
    }

    pub async fn close_tunnel(&self, token: &str, tunnel_id: &str) -> reqwest::Result<u16> {
        let resp = self
            .client
            .delete(format!("{}/api/tunnels/{}", self.base_url, tunnel_id))
            .bearer_auth(token)
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    /// Fetch the active region list from `GET /api/regions` (no auth required).
    pub async fn list_regions(&self) -> reqwest::Result<Value> {
        self.client
            .get(format!("{}/api/regions", self.base_url))
            .send()
            .await?
            .json()
            .await
    }

    pub async fn get_history(
        &self,
        token: &str,
        limit: u64,
        protocol: Option<&str>,
    ) -> reqwest::Result<Value> {
        let mut url = format!("{}/api/history?limit={}", self.base_url, limit);
        if let Some(p) = protocol {
            url.push_str(&format!("&protocol={p}"));
        }
        self.client
            .get(url)
            .bearer_auth(token)
            .send()
            .await?
            .json()
            .await
    }
}
