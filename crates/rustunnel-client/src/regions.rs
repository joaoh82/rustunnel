//! Multi-region support: region discovery, latency probing, and server selection.
//!
//! Resolution order when choosing a server:
//!   1. `--region <id>`  → connect directly to `<id>.edge.rustunnel.com:4040`, no probe
//!   2. `--region auto` or `region: auto` in config → probe all regions, pick nearest
//!   3. No region preference → return `config.server` unchanged (backward compat)
//!
//! Region list is loaded via three-tier fallback:
//!   1. `~/.rustunnel/regions.json` if fresh (< 24 h)
//!   2. `GET https://<host>:8443/api/regions` (dashboard API, no auth required)
//!   3. Hardcoded built-in list compiled into the binary

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionInfo {
    pub id: String,
    pub name: String,
    pub location: String,
    pub host: String,
    pub control_port: u16,
}

/// Wire format returned by `GET /api/regions`.
#[derive(Debug, Deserialize)]
struct ApiRegion {
    id: String,
    name: String,
    location: String,
    host: String,
    control_port: u16,
    active: bool,
}

#[derive(Serialize, Deserialize)]
struct RegionCache {
    fetched_at: DateTime<Utc>,
    regions: Vec<RegionInfo>,
}

// ── built-in fallback ─────────────────────────────────────────────────────────

fn builtin_regions() -> Vec<RegionInfo> {
    vec![
        RegionInfo {
            id: "eu".into(),
            name: "Europe".into(),
            location: "Helsinki, FI".into(),
            host: "eu.edge.rustunnel.com".into(),
            control_port: 4040,
        },
        RegionInfo {
            id: "us".into(),
            name: "US East".into(),
            location: "Ashburn, VA".into(),
            host: "us.edge.rustunnel.com".into(),
            control_port: 4040,
        },
        RegionInfo {
            id: "ap".into(),
            name: "Asia Pacific".into(),
            location: "Singapore".into(),
            host: "ap.edge.rustunnel.com".into(),
            control_port: 4040,
        },
    ]
}

// ── cache ─────────────────────────────────────────────────────────────────────

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".rustunnel").join("regions.json"))
}

fn load_cache() -> Option<Vec<RegionInfo>> {
    let raw = std::fs::read_to_string(cache_path()?).ok()?;
    let cache: RegionCache = serde_json::from_str(&raw).ok()?;
    let age = Utc::now().signed_duration_since(cache.fetched_at);
    if age < chrono::Duration::hours(24) {
        Some(cache.regions)
    } else {
        None
    }
}

fn save_cache(regions: &[RegionInfo]) {
    let Some(path) = cache_path() else { return };
    let cache = RegionCache { fetched_at: Utc::now(), regions: regions.to_vec() };
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(path, json);
    }
}

// ── region list resolution ────────────────────────────────────────────────────

/// Load the region list: cache → API → built-in fallback.
async fn load_regions(bootstrap_host: &str, insecure: bool) -> Vec<RegionInfo> {
    if let Some(cached) = load_cache() {
        return cached;
    }
    match fetch_regions(bootstrap_host, insecure).await {
        Ok(regions) => {
            save_cache(&regions);
            regions
        }
        Err(_) => builtin_regions(),
    }
}

/// Fetch the region list from the dashboard API of the given host.
async fn fetch_regions(host: &str, insecure: bool) -> Result<Vec<RegionInfo>, reqwest::Error> {
    let url = format!("https://{host}:8443/api/regions");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(insecure)
        .timeout(Duration::from_secs(5))
        .build()?;
    let regions: Vec<ApiRegion> = client.get(&url).send().await?.json().await?;
    Ok(regions
        .into_iter()
        .filter(|r| r.active)
        .map(|r| RegionInfo {
            id: r.id,
            name: r.name,
            location: r.location,
            host: r.host,
            control_port: r.control_port,
        })
        .collect())
}

// ── latency probing ───────────────────────────────────────────────────────────

/// TCP-connect to host:port and return the round-trip time.
/// Returns 10 s on timeout or connection error so that unreachable regions
/// never win the auto-select.
async fn probe_latency(host: &str, port: u16) -> Duration {
    let addr = format!("{host}:{port}");
    let start = Instant::now();
    match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => start.elapsed(),
        _ => Duration::from_secs(10),
    }
}

/// Probe all regions in parallel and return the server address of the nearest.
async fn auto_select(bootstrap_host: &str, insecure: bool) -> String {
    let regions = load_regions(bootstrap_host, insecure).await;

    eprint!("  Selecting nearest region…");

    let probes: Vec<_> = regions
        .iter()
        .map(|r| {
            let host = r.host.clone();
            let port = r.control_port;
            async move { probe_latency(&host, port).await }
        })
        .collect();

    let latencies = futures_util::future::join_all(probes).await;

    // Print all results on one line
    for (r, d) in regions.iter().zip(latencies.iter()) {
        eprint!(" {} {}ms ·", r.id, d.as_millis());
    }

    let (best, best_ms) = regions
        .iter()
        .zip(latencies.iter())
        .min_by_key(|(_, d)| d.as_millis())
        .map(|(r, d)| (r, d.as_millis()))
        .expect("region list is never empty");

    eprintln!(" → {} ({}) {}ms", best.id, best.location, best_ms);

    format!("{}:{}", best.host, best.control_port)
}

// ── public API ────────────────────────────────────────────────────────────────

/// Extract the hostname from a `host:port` string.
fn extract_host(server: &str) -> &str {
    server.split(':').next().unwrap_or(server)
}

/// Resolve the control-plane server address to connect to.
///
/// - `config_server`: current value of `ClientConfig::server` (e.g. `"eu.edge.rustunnel.com:4040"`)
/// - `region_flag`: value of `--region` CLI flag, if provided
/// - `config_region`: value of `region:` from config file, if set
/// - `insecure`: whether to skip TLS verification for the region API fetch
///
/// Returns a `"host:port"` string ready to pass to `control::connect`.
pub async fn resolve_server(
    config_server: &str,
    region_flag: Option<&str>,
    config_region: Option<&str>,
    insecure: bool,
) -> String {
    let effective = region_flag.or(config_region);

    match effective {
        // Explicit region ID: connect directly, skip probing
        Some(r) if r != "auto" => {
            let regions = builtin_regions();
            if let Some(info) = regions.iter().find(|ri| ri.id == r) {
                eprintln!("  Region: {} ({})", info.name, info.location);
                return format!("{}:{}", info.host, info.control_port);
            }
            // Unknown ID — warn and fall back to auto-select
            eprintln!("  Unknown region '{r}', falling back to auto-select");
            let host = extract_host(config_server);
            auto_select(host, insecure).await
        }

        // "auto" or any other value: probe and pick nearest
        Some(_) => {
            let host = extract_host(config_server);
            auto_select(host, insecure).await
        }

        // No preference: return config.server unchanged (backward compat)
        None => config_server.to_string(),
    }
}
