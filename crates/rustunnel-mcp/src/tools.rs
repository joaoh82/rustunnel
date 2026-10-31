//! Tool definitions, input schemas, and execution logic for all MCP tools.
//!
//! Tools implemented here:
//!   - `create_tunnel` — spawns the `rustunnel` CLI and polls the API for the
//!     URL. Supports HTTP/TCP/UDP, P2P (peer-to-peer with a shared secret), and
//!     load-balanced groups with optional health checks.
//!   - `list_tunnels` — GET /api/tunnels wrapper
//!   - `close_tunnel` — DELETE /api/tunnels/:id + kills spawned process
//!   - `get_connection_info` — returns CLI command / config (no API call)
//!   - `list_regions` — GET /api/regions wrapper
//!   - `get_tunnel_history` — GET /api/history wrapper
//!
//! ## Authentication
//!
//! Every authenticated tool accepts an optional `token` argument. When omitted,
//! the token configured via the `RUSTUNNEL_TOKEN` environment variable (read at
//! startup into [`State::token`]) is used. This lets an agent configure the
//! token once in its MCP client config instead of passing it on every call.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::mcp::{tool_err, tool_ok};
use crate::State;

// ── argument helpers ──────────────────────────────────────────────────────────

/// Extract a required string argument.
macro_rules! req_str {
    ($args:expr, $key:expr) => {
        match $args.get($key).and_then(Value::as_str) {
            Some(v) => v,
            None => return tool_err(format!("missing required argument: {}", $key)),
        }
    };
}

/// Guidance returned when no token can be resolved from the argument or env.
const NO_TOKEN_HELP: &str = "No API token available. Set the RUSTUNNEL_TOKEN \
    environment variable in your MCP client config, or pass a `token` argument. \
    Get a free token at https://rustunnel.com (Dashboard → API Keys), or for a \
    self-hosted server create one with `rustunnel token create`.";

/// Choose a token: an explicit argument wins, otherwise fall back to the
/// environment-configured default. Pure so it can be unit-tested.
fn pick_token(arg: Option<&str>, env: Option<&str>) -> Option<String> {
    arg.filter(|t| !t.trim().is_empty())
        .or(env.filter(|t| !t.trim().is_empty()))
        .map(str::to_string)
}

/// Resolve the API token for a tool call from the `token` argument or the
/// `RUSTUNNEL_TOKEN`-derived default.
fn resolve_token(args: &Value, state: &State) -> Option<String> {
    pick_token(
        args.get("token").and_then(Value::as_str),
        state.token.as_deref(),
    )
}

/// Locate the `rustunnel` CLI binary that `create_tunnel` spawns.
///
/// Resolution order:
/// 1. `RUSTUNNEL_CLI` env var — set by packaged installs (e.g. the MCPB
///    bundle points it at the CLI shipped inside the bundle).
/// 2. A `rustunnel` binary sitting next to this `rustunnel-mcp` executable —
///    covers bundles and tarball installs without any configuration.
/// 3. Plain `rustunnel`, resolved through `PATH` (the historical behaviour).
fn resolve_cli_path() -> PathBuf {
    resolve_cli_path_from(std::env::var("RUSTUNNEL_CLI").ok().as_deref())
}

/// Pure resolution logic, split out so it can be unit-tested without
/// mutating process environment (env mutation races with parallel tests).
fn resolve_cli_path_from(env_override: Option<&str>) -> PathBuf {
    if let Some(p) = env_override.map(str::trim).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) {
                "rustunnel.exe"
            } else {
                "rustunnel"
            };
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("rustunnel")
}

// ── tool definitions (returned by tools/list) ─────────────────────────────────

pub fn tool_definitions() -> Vec<Value> {
    let health_check_schema = serde_json::json!({
        "type": "object",
        "description": "Optional client-side health probe. The client probes the \
            local service and reports health so the server routes around a sick \
            backend. Only meaningful for load-balanced tunnels (group set).",
        "properties": {
            "type":          { "type": "string", "enum": ["tcp", "http"], "description": "Probe kind: 'tcp' opens a connection, 'http' issues a GET" },
            "path":          { "type": "string", "description": "Path to GET (required when type='http'), e.g. /health" },
            "interval_secs": { "type": "integer", "description": "Probe period in seconds (default 10)" },
            "timeout_secs":  { "type": "integer", "description": "Per-probe timeout in seconds (default 3)" },
            "max_failed":    { "type": "integer", "description": "Consecutive failures before marking unhealthy (default 3)" },
            "expect_2xx":    { "type": "boolean", "description": "When false, any HTTP response counts as healthy (default true)" },
            "alert_webhook": { "type": "string", "description": "URL the server POSTs to when this tunnel's group drops to 0 healthy members" }
        },
        "required": ["type"]
    });

    vec![
        serde_json::json!({
            "name": "create_tunnel",
            "description": "Open a tunnel to a locally running service and get a public URL. \
                Use this when the user or task needs a public URL for a service running \
                locally — webhook testing, sharing a dev server, or letting another agent \
                or device reach localhost. \
                Spawns the rustunnel CLI as a subprocess — the tunnel stays open until \
                close_tunnel is called or the MCP server exits.\n\
                \n\
                Modes:\n\
                - protocol='http'  → public https:// URL (web/API services)\n\
                - protocol='tcp'   → public host:port (databases, SSH, raw TCP)\n\
                - protocol='udp'   → public host:port (game servers, DNS)\n\
                - protocol='p2p'   → peer-to-peer; requires `secret` plus `peer_name` \
                                     (to publish) or `peer_target` (to connect)\n\
                - load balancing   → set `group` + `group_key` (http/tcp only) to join \
                                     this tunnel into a load-balanced pool; add \
                                     `health_check` to auto-remove sick backends.\n\
                \n\
                The token may be omitted if RUSTUNNEL_TOKEN is configured.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token":      { "type": "string", "description": "API token. Optional if RUSTUNNEL_TOKEN is set in the MCP client config." },
                    "local_port": { "type": "integer", "description": "Local port the service is listening on, e.g. 3000" },
                    "protocol":   { "type": "string", "enum": ["http", "tcp", "udp", "p2p"], "description": "Tunnel type" },
                    "subdomain":  { "type": "string", "description": "Custom subdomain (HTTP tunnels only). Server assigns a random one if omitted." },
                    "region":     { "type": "string", "description": "Region ID (e.g. 'eu', 'us', 'ap'). Omit to auto-select the nearest by latency. Use list_regions to see options." },
                    "local_host": { "type": "string", "description": "Local hostname to forward to (default 'localhost'). Use to expose a service on another host in your network." },
                    "secret":     { "type": "string", "description": "Shared secret for P2P (protocol='p2p'). Publisher and subscriber must use the same value." },
                    "peer_name":  { "type": "string", "description": "P2P publisher mode: publish the local service under this name." },
                    "peer_target":{ "type": "string", "description": "P2P subscriber mode: connect to a published P2P tunnel with this name." },
                    "group":      { "type": "string", "description": "Load-balancing pool name. Tunnels sharing the same (group, group_key) form one pool (http/tcp only)." },
                    "group_key":  { "type": "string", "description": "Shared secret for the load-balancing group. Members of one pool must agree on this value." },
                    "health_check": health_check_schema
                },
                "required": ["local_port", "protocol"]
            }
        }),
        serde_json::json!({
            "name": "list_tunnels",
            "description": "List all tunnels currently open on this server. Returns the public \
                URL, protocol, and traffic count for each active tunnel. Use this when you \
                need to check whether a tunnel is still running, recover the public URL of \
                an existing tunnel, or find a tunnel_id to pass to close_tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": { "type": "string", "description": "API token. Optional if RUSTUNNEL_TOKEN is set." }
                }
            }
        }),
        serde_json::json!({
            "name": "close_tunnel",
            "description": "Force-close a specific tunnel by its ID. The public URL stops \
                working immediately. Use this when a tunnel is no longer needed or public \
                access must be revoked right away — e.g. after a webhook test or demo \
                finishes. Use list_tunnels to find the tunnel_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token":     { "type": "string", "description": "API token. Optional if RUSTUNNEL_TOKEN is set." },
                    "tunnel_id": { "type": "string", "description": "UUID of the tunnel to close" }
                },
                "required": ["tunnel_id"]
            }
        }),
        serde_json::json!({
            "name": "get_connection_info",
            "description": "Get the CLI command (and, for load-balanced tunnels, a config file) \
                needed to create a tunnel manually — without spawning anything. Use this when \
                the MCP server cannot spawn subprocesses (e.g. cloud sandbox), when the tunnel \
                must outlive the MCP server, or when you prefer to run the client yourself. \
                Accepts the same arguments as create_tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token":      { "type": "string", "description": "API token. Optional if RUSTUNNEL_TOKEN is set." },
                    "local_port": { "type": "integer", "description": "Local port the service is listening on" },
                    "protocol":   { "type": "string", "enum": ["http", "tcp", "udp", "p2p"], "description": "Tunnel type" },
                    "subdomain":  { "type": "string", "description": "Custom subdomain (HTTP only)" },
                    "region":     { "type": "string", "description": "Region ID (e.g. 'eu', 'us', 'ap'). Omit to auto-select." },
                    "local_host": { "type": "string", "description": "Local hostname to forward to (default 'localhost')" },
                    "secret":     { "type": "string", "description": "Shared secret for P2P (protocol='p2p')" },
                    "peer_name":  { "type": "string", "description": "P2P publisher mode: publish under this name" },
                    "peer_target":{ "type": "string", "description": "P2P subscriber mode: connect to this name" },
                    "group":      { "type": "string", "description": "Load-balancing pool name (http/tcp only)" },
                    "group_key":  { "type": "string", "description": "Shared secret for the load-balancing group" },
                    "health_check": health_check_schema
                },
                "required": ["local_port", "protocol"]
            }
        }),
        serde_json::json!({
            "name": "list_regions",
            "description": "List available tunnel server regions with their IDs, names, \
                and locations. Use this when you need to pick a specific `region` for \
                create_tunnel or check which edge locations exist. \
                No authentication required.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "get_tunnel_history",
            "description": "Retrieve the history of past tunnels, including their duration and \
                which token opened them. Use this when auditing past tunnel activity or \
                debugging why a tunnel dropped or is no longer listed by list_tunnels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token":    { "type": "string", "description": "API token. Optional if RUSTUNNEL_TOKEN is set." },
                    "protocol": { "type": "string", "enum": ["http", "tcp", "udp", "p2p"], "description": "Optional: filter by protocol. Omit for all." },
                    "limit":    { "type": "integer", "default": 25, "description": "Maximum number of entries to return" }
                }
            }
        }),
    ]
}

// ── dispatcher ────────────────────────────────────────────────────────────────

pub async fn dispatch(name: &str, args: &Value, state: &Arc<State>) -> Value {
    match name {
        "create_tunnel" => create_tunnel(args, state).await,
        "list_tunnels" => list_tunnels(args, state).await,
        "close_tunnel" => close_tunnel(args, state).await,
        "get_connection_info" => get_connection_info(args, state),
        "get_tunnel_history" => get_tunnel_history(args, state).await,
        "list_regions" => list_regions(state).await,
        _ => tool_err(format!("unknown tool: {name}")),
    }
}

// ── command / config builders (pure, unit-tested) ─────────────────────────────

/// Build the `rustunnel <proto> <port> ...` argument vector for an inline
/// HTTP/TCP/UDP tunnel (everything after the binary name).
#[allow(clippy::too_many_arguments)]
fn build_inline_args(
    protocol: &str,
    local_port: u64,
    server: &str,
    token: &str,
    insecure: bool,
    subdomain: Option<&str>,
    region: Option<&str>,
    local_host: Option<&str>,
) -> Vec<String> {
    let mut a = vec![
        protocol.to_string(),
        local_port.to_string(),
        "--server".into(),
        server.into(),
        "--token".into(),
        token.into(),
    ];
    if insecure {
        a.push("--insecure".into());
    }
    if let Some(s) = subdomain {
        a.push("--subdomain".into());
        a.push(s.into());
    }
    if let Some(r) = region {
        a.push("--region".into());
        a.push(r.into());
    }
    if let Some(h) = local_host {
        a.push("--local_host".into());
        a.push(h.into());
    }
    a
}

/// Build the `rustunnel p2p <port> ...` argument vector. Returns an error
/// string when the required P2P arguments are missing or contradictory.
#[allow(clippy::too_many_arguments)]
fn build_p2p_args(
    local_port: u64,
    server: &str,
    token: &str,
    insecure: bool,
    secret: Option<&str>,
    peer_name: Option<&str>,
    peer_target: Option<&str>,
    region: Option<&str>,
    local_host: Option<&str>,
) -> Result<Vec<String>, String> {
    let secret = secret.filter(|s| !s.is_empty()).ok_or_else(|| {
        "P2P tunnels require a `secret` (publisher and subscriber must match).".to_string()
    })?;

    let mut a = vec![
        "p2p".to_string(),
        local_port.to_string(),
        "--secret".into(),
        secret.into(),
        "--server".into(),
        server.into(),
        "--token".into(),
        token.into(),
    ];

    match (peer_name, peer_target) {
        (Some(n), None) => {
            a.push("--name".into());
            a.push(n.into());
        }
        (None, Some(t)) => {
            a.push("--target".into());
            a.push(t.into());
        }
        (Some(_), Some(_)) => {
            return Err("P2P: set either `peer_name` (publish) or `peer_target` \
                (connect), not both."
                .into())
        }
        (None, None) => {
            return Err(
                "P2P: set `peer_name` to publish a service, or `peer_target` \
                to connect to one."
                    .into(),
            )
        }
    }

    if insecure {
        a.push("--insecure".into());
    }
    if let Some(r) = region {
        a.push("--region".into());
        a.push(r.into());
    }
    if let Some(h) = local_host {
        a.push("--local_host".into());
        a.push(h.into());
    }
    Ok(a)
}

/// Double-quote and escape a string for safe embedding as a YAML scalar.
fn yaml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render the indented `health_check:` field lines (6-space indent) from the
/// `health_check` argument object. Returns an empty string when `hc` is not an
/// object. Pure so it can be unit-tested.
fn health_check_yaml(hc: &Value) -> String {
    let Some(map) = hc.as_object() else {
        return String::new();
    };
    let mut out = String::from("    health_check:\n");
    if let Some(t) = map.get("type").and_then(Value::as_str) {
        out.push_str(&format!("      type: {}\n", yaml_str(t)));
    }
    if let Some(p) = map.get("path").and_then(Value::as_str) {
        out.push_str(&format!("      path: {}\n", yaml_str(p)));
    }
    for (key, json_key) in [
        ("interval_secs", "interval_secs"),
        ("timeout_secs", "timeout_secs"),
        ("max_failed", "max_failed"),
    ] {
        if let Some(n) = map.get(json_key).and_then(Value::as_u64) {
            out.push_str(&format!("      {key}: {n}\n"));
        }
    }
    if let Some(b) = map.get("expect_2xx").and_then(Value::as_bool) {
        out.push_str(&format!("      expect_2xx: {b}\n"));
    }
    if let Some(w) = map.get("alert_webhook").and_then(Value::as_str) {
        out.push_str(&format!("      alert_webhook: {}\n", yaml_str(w)));
    }
    out
}

/// Build a `rustunnel start` config file for one load-balanced tunnel member.
/// Pure so it can be unit-tested.
#[allow(clippy::too_many_arguments)]
fn build_lb_config_yaml(
    server: &str,
    token: &str,
    insecure: bool,
    protocol: &str,
    local_port: u64,
    local_host: Option<&str>,
    subdomain: Option<&str>,
    region: Option<&str>,
    group: &str,
    group_key: &str,
    health_check: Option<&Value>,
) -> String {
    let mut out = String::from("# Generated by rustunnel-mcp for a load-balanced tunnel.\n");
    out.push_str(&format!("server: {}\n", yaml_str(server)));
    out.push_str(&format!("auth_token: {}\n", yaml_str(token)));
    if insecure {
        out.push_str("insecure: true\n");
    }
    if let Some(r) = region {
        out.push_str(&format!("region: {}\n", yaml_str(r)));
    }
    out.push_str("\ntunnels:\n  lb:\n");
    out.push_str(&format!("    proto: {protocol}\n"));
    out.push_str(&format!("    local_port: {local_port}\n"));
    out.push_str(&format!(
        "    local_host: {}\n",
        yaml_str(local_host.unwrap_or("localhost"))
    ));
    if let Some(s) = subdomain {
        out.push_str(&format!("    subdomain: {}\n", yaml_str(s)));
    }
    out.push_str(&format!("    group: {}\n", yaml_str(group)));
    out.push_str(&format!("    group_key: {}\n", yaml_str(group_key)));
    if let Some(hc) = health_check {
        out.push_str(&health_check_yaml(hc));
    }
    out
}

/// Generate a unique temp path for a load-balanced tunnel's config file.
fn temp_config_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "rustunnel-mcp-{}-{nanos}-{n}.yml",
        std::process::id()
    ))
}

/// Plan: the CLI args to run and an optional temp config file backing them.
struct SpawnPlan {
    args: Vec<String>,
    temp_config: Option<PathBuf>,
}

/// Decide how to launch `rustunnel` for a create_tunnel / get_connection_info
/// request. Writes a temp config file for load-balanced tunnels.
fn plan_spawn(args: &Value, token: &str, state: &State) -> Result<SpawnPlan, String> {
    let protocol = args
        .get("protocol")
        .and_then(Value::as_str)
        .ok_or("missing required argument: protocol")?;
    let local_port = args
        .get("local_port")
        .and_then(Value::as_u64)
        .ok_or("missing required argument: local_port")?;

    let subdomain = args.get("subdomain").and_then(Value::as_str);
    let region = args.get("region").and_then(Value::as_str);
    let local_host = args.get("local_host").and_then(Value::as_str);
    let group = args.get("group").and_then(Value::as_str);
    let group_key = args.get("group_key").and_then(Value::as_str);
    let health_check = args.get("health_check").filter(|v| v.is_object());

    // Load-balanced tunnel: requires group + group_key, only http/tcp.
    if group.is_some() || group_key.is_some() {
        let group = group.ok_or("load balancing requires `group`")?;
        let group_key = group_key.ok_or("load balancing requires `group_key`")?;
        if protocol != "http" && protocol != "tcp" {
            return Err("load balancing is only supported for http and tcp tunnels".into());
        }
        let yaml = build_lb_config_yaml(
            &state.server_addr,
            token,
            state.insecure,
            protocol,
            local_port,
            local_host,
            subdomain,
            region,
            group,
            group_key,
            health_check,
        );
        let path = temp_config_path();
        std::fs::write(&path, yaml)
            .map_err(|e| format!("failed to write temp config {}: {e}", path.display()))?;
        let args = vec![
            "start".to_string(),
            "--config".into(),
            path.to_string_lossy().into_owned(),
        ];
        return Ok(SpawnPlan {
            args,
            temp_config: Some(path),
        });
    }

    // P2P tunnel.
    if protocol == "p2p" {
        let cli = build_p2p_args(
            local_port,
            &state.server_addr,
            token,
            state.insecure,
            args.get("secret").and_then(Value::as_str),
            args.get("peer_name").and_then(Value::as_str),
            args.get("peer_target").and_then(Value::as_str),
            region,
            local_host,
        )?;
        return Ok(SpawnPlan {
            args: cli,
            temp_config: None,
        });
    }

    // Plain HTTP/TCP/UDP tunnel.
    let cli = build_inline_args(
        protocol,
        local_port,
        &state.server_addr,
        token,
        state.insecure,
        subdomain,
        region,
        local_host,
    );
    Ok(SpawnPlan {
        args: cli,
        temp_config: None,
    })
}

// ── tool implementations ──────────────────────────────────────────────────────

async fn create_tunnel(args: &Value, state: &Arc<State>) -> Value {
    let token = match resolve_token(args, state) {
        Some(t) => t,
        None => return tool_err(NO_TOKEN_HELP),
    };

    let plan = match plan_spawn(args, &token, state) {
        Ok(p) => p,
        Err(e) => return tool_err(e),
    };

    // Snapshot existing tunnel IDs before spawning so we can identify the new one.
    let before_ids: HashSet<String> = match state.api.list_tunnels(&token).await {
        Ok(ts) => ts.iter().map(|t| t.tunnel_id.clone()).collect(),
        Err(e) => {
            cleanup_temp(&plan.temp_config);
            return tool_err(format!("failed to query current tunnels: {e}"));
        }
    };

    // Build and spawn the CLI command.
    let cli = resolve_cli_path();
    let mut cmd = tokio::process::Command::new(&cli);
    cmd.args(&plan.args)
        // The edge stamps utm_source from this. Released CLIs ignore it and
        // still print the cli URL; we rewrite that below.
        .env("RUSTUNNEL_SURFACE", "mcp")
        // Suppress CLI stdout so it doesn't interfere with MCP stdio.
        // stderr is captured so a Hobby-limit rejection becomes the tool error
        // instead of a generic timeout.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child_opt = match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            cleanup_temp(&plan.temp_config);
            return tool_err(format!(
                "failed to spawn rustunnel ({}): {e}. \
                Install the 'rustunnel' CLI and put it on PATH, \
                or point the RUSTUNNEL_CLI env var at the binary.",
                cli.display()
            ));
        }
    };

    let stderr_buf = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_task = child_opt.as_mut().and_then(|child| {
        let stderr = child.stderr.take()?;
        let buf = Arc::clone(&stderr_buf);
        Some(tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut guard = buf.lock().unwrap_or_else(|p| p.into_inner());
                        if guard.len() < 8 * 1024 {
                            guard.push_str(&line);
                        }
                    }
                }
            }
        }))
    });

    // Poll the API for up to 15 seconds waiting for the new tunnel to appear.
    // A fast CLI exit (Hobby limit, bad token) ends the wait immediately.
    let mut cli_exited = false;
    for _ in 0..30 {
        if let Some(child) = child_opt.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    cli_exited = true;
                    break;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("failed to poll rustunnel process: {e}"),
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        match state.api.list_tunnels(&token).await {
            Ok(tunnels) => {
                let new = tunnels
                    .into_iter()
                    .find(|t| !before_ids.contains(&t.tunnel_id));

                if let Some(tunnel) = new {
                    let tunnel_id = tunnel.tunnel_id.clone();
                    state
                        .tunnel_manager
                        .insert(
                            tunnel_id.clone(),
                            child_opt.take().unwrap(),
                            plan.temp_config.clone(),
                        )
                        .await;

                    return tool_ok(
                        serde_json::to_string_pretty(&serde_json::json!({
                            "public_url": tunnel.public_url,
                            "tunnel_id":  tunnel_id,
                            "protocol":   tunnel.protocol,
                        }))
                        .unwrap(),
                    );
                }
            }
            Err(e) => {
                tracing::warn!("poll error while waiting for tunnel: {e}");
            }
        }
    }

    // Timeout or CLI exit. Kill a still-running subprocess and drop the temp config.
    if let Some(mut child) = child_opt {
        if !cli_exited {
            let _ = child.start_kill();
        }
        let _ = child.wait().await;
    }
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    cleanup_temp(&plan.temp_config);

    let collected = stderr_buf.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if cli_exited {
        return tool_err(cli_exit_tool_error(&collected));
    }
    if let Some(text) = upgrade_text_from_cli_output(&collected) {
        return tool_err(text);
    }

    tool_err(
        "timeout: tunnel did not appear within 15 seconds. \
        Check that the server address and token are correct.",
    )
}

/// Pull the Hobby upgrade copy out of CLI stderr and stamp `utm_source=mcp`.
///
/// Released CLIs print `error: tunnel error: {server message}`. The server
/// message already contains the full upgrade URL with `utm_source=cli`.
fn upgrade_text_from_cli_output(raw: &str) -> Option<String> {
    let text = raw.replace("\r\n", "\n");
    let start = text.find("Hobby plan")?;
    let mut lines: Vec<String> = Vec::new();
    for (i, line) in text[start..].lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        if i > 0 && !line.starts_with("Upgrade:") && !line.starts_with("https://") {
            break;
        }
        lines.push(line.to_string());
        if line.starts_with("Upgrade:") || line.starts_with("https://") {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut msg = lines.join("\n");
    if let Some(idx) = msg.find("Hobby plan") {
        msg = msg[idx..].to_string();
    }
    Some(msg.replace("utm_source=cli", "utm_source=mcp"))
}

/// Tool error when the CLI process exits before a tunnel is registered.
fn cli_exit_tool_error(stderr: &str) -> String {
    if let Some(text) = upgrade_text_from_cli_output(stderr) {
        return text;
    }
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("error:"))
        .map(str::trim);
    match line {
        Some(line) => format!("rustunnel exited before the tunnel was registered.\n{line}"),
        None => "rustunnel exited before the tunnel was registered.".into(),
    }
}

fn cleanup_temp(path: &Option<PathBuf>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

async fn list_tunnels(args: &Value, state: &Arc<State>) -> Value {
    let token = match resolve_token(args, state) {
        Some(t) => t,
        None => return tool_err(NO_TOKEN_HELP),
    };

    match state.api.list_tunnels(&token).await {
        Ok(tunnels) => tool_ok(serde_json::to_string_pretty(&tunnels).unwrap()),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

async fn close_tunnel(args: &Value, state: &Arc<State>) -> Value {
    let token = match resolve_token(args, state) {
        Some(t) => t,
        None => return tool_err(NO_TOKEN_HELP),
    };
    let tunnel_id = req_str!(args, "tunnel_id");

    // Kill the spawned process if we were the ones that opened this tunnel.
    state.tunnel_manager.kill(tunnel_id).await;

    // Close via the REST API (authoritative close — removes it from routing).
    match state.api.close_tunnel(&token, tunnel_id).await {
        Ok(204) | Ok(200) => tool_ok("Tunnel closed successfully."),
        Ok(404) => tool_err("Tunnel not found. It may have already been closed."),
        Ok(401) => tool_err("Authentication failed — check your token."),
        Ok(status) => tool_err(format!("Unexpected status from server: {status}")),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

fn get_connection_info(args: &Value, state: &Arc<State>) -> Value {
    let token = match resolve_token(args, state) {
        Some(t) => t,
        None => return tool_err(NO_TOKEN_HELP),
    };

    let plan = match plan_spawn(args, &token, state) {
        Ok(p) => p,
        Err(e) => return tool_err(e),
    };

    let cli_command = format!("rustunnel {}", plan.args.join(" "));

    let mut info = serde_json::json!({
        "cli_command": cli_command,
        "server":      state.server_addr,
        "install_url": "https://github.com/joaoh82/rustunnel/releases/latest",
    });

    // For a load-balanced tunnel the args reference a temp file; surface the
    // config contents and a stable path the user can save it to instead.
    if let Some(path) = &plan.temp_config {
        if let Ok(contents) = std::fs::read_to_string(path) {
            info["config_yaml"] = Value::String(contents);
            info["cli_command"] = Value::String(
                "rustunnel start --config ~/.rustunnel/lb.yml  # save config_yaml there first"
                    .to_string(),
            );
        }
        // This was only generated to render the command; remove it now.
        let _ = std::fs::remove_file(path);
    }

    if let Some(r) = args.get("region").and_then(Value::as_str) {
        info["region"] = Value::String(r.to_string());
    }

    tool_ok(serde_json::to_string_pretty(&info).unwrap())
}

async fn list_regions(state: &Arc<State>) -> Value {
    match state.api.list_regions().await {
        Ok(regions) => tool_ok(serde_json::to_string_pretty(&regions).unwrap()),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

async fn get_tunnel_history(args: &Value, state: &Arc<State>) -> Value {
    let token = match resolve_token(args, state) {
        Some(t) => t,
        None => return tool_err(NO_TOKEN_HELP),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(25);
    let protocol = args.get("protocol").and_then(Value::as_str);

    match state.api.get_history(&token, limit, protocol).await {
        Ok(history) => tool_ok(serde_json::to_string_pretty(&history).unwrap()),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pick_token_prefers_argument() {
        assert_eq!(
            pick_token(Some("arg"), Some("env")),
            Some("arg".to_string())
        );
    }

    #[test]
    fn pick_token_falls_back_to_env() {
        assert_eq!(pick_token(None, Some("env")), Some("env".to_string()));
        assert_eq!(pick_token(Some(""), Some("env")), Some("env".to_string()));
        assert_eq!(pick_token(Some("  "), Some("env")), Some("env".to_string()));
    }

    #[test]
    fn pick_token_none_when_unset() {
        assert_eq!(pick_token(None, None), None);
        assert_eq!(pick_token(Some(""), Some("  ")), None);
    }

    #[test]
    fn inline_args_minimal() {
        let a = build_inline_args("http", 3000, "srv:4040", "tok", false, None, None, None);
        assert_eq!(
            a,
            vec!["http", "3000", "--server", "srv:4040", "--token", "tok"]
        );
    }

    #[test]
    fn inline_args_full() {
        let a = build_inline_args(
            "http",
            5173,
            "srv:4040",
            "tok",
            true,
            Some("myapp"),
            Some("eu"),
            Some("0.0.0.0"),
        );
        assert!(a.contains(&"--insecure".to_string()));
        assert!(a.windows(2).any(|w| w == ["--subdomain", "myapp"]));
        assert!(a.windows(2).any(|w| w == ["--region", "eu"]));
        assert!(a.windows(2).any(|w| w == ["--local_host", "0.0.0.0"]));
    }

    #[test]
    fn p2p_publisher_args() {
        let a = build_p2p_args(
            3000,
            "srv:4040",
            "tok",
            false,
            Some("s3cret"),
            Some("my-svc"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(a[0], "p2p");
        assert!(a.windows(2).any(|w| w == ["--secret", "s3cret"]));
        assert!(a.windows(2).any(|w| w == ["--name", "my-svc"]));
        assert!(!a.iter().any(|x| x == "--target"));
    }

    #[test]
    fn p2p_subscriber_args() {
        let a = build_p2p_args(
            8000,
            "srv:4040",
            "tok",
            false,
            Some("s3cret"),
            None,
            Some("my-svc"),
            None,
            None,
        )
        .unwrap();
        assert!(a.windows(2).any(|w| w == ["--target", "my-svc"]));
        assert!(!a.iter().any(|x| x == "--name"));
    }

    #[test]
    fn p2p_requires_secret() {
        let err = build_p2p_args(
            3000,
            "srv",
            "tok",
            false,
            None,
            Some("svc"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("secret"));
    }

    #[test]
    fn p2p_requires_peer_role() {
        let err = build_p2p_args(3000, "srv", "tok", false, Some("s"), None, None, None, None)
            .unwrap_err();
        assert!(err.contains("peer_name") || err.contains("peer_target"));

        let err = build_p2p_args(
            3000,
            "srv",
            "tok",
            false,
            Some("s"),
            Some("a"),
            Some("b"),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("not both"));
    }

    #[test]
    fn lb_config_yaml_contains_group_and_health() {
        let hc = json!({
            "type": "http",
            "path": "/health",
            "interval_secs": 5,
            "expect_2xx": false
        });
        let yaml = build_lb_config_yaml(
            "srv:4040",
            "tok",
            false,
            "http",
            3000,
            None,
            Some("pool"),
            None,
            "web",
            "shared-key",
            Some(&hc),
        );
        assert!(yaml.contains("auth_token: \"tok\""));
        assert!(yaml.contains("server: \"srv:4040\""));
        assert!(yaml.contains("proto: http"));
        assert!(yaml.contains("group: \"web\""));
        assert!(yaml.contains("group_key: \"shared-key\""));
        assert!(yaml.contains("subdomain: \"pool\""));
        assert!(yaml.contains("health_check:"));
        assert!(yaml.contains("type: \"http\""));
        assert!(yaml.contains("path: \"/health\""));
        assert!(yaml.contains("interval_secs: 5"));
        assert!(yaml.contains("expect_2xx: false"));
        // No region line when none requested.
        assert!(!yaml.contains("region:"));
    }

    #[test]
    fn lb_config_yaml_insecure_and_region() {
        let yaml = build_lb_config_yaml(
            "localhost:4040",
            "tok",
            true,
            "tcp",
            5432,
            Some("db.internal"),
            None,
            Some("eu"),
            "dbpool",
            "k",
            None,
        );
        assert!(yaml.contains("insecure: true"));
        assert!(yaml.contains("region: \"eu\""));
        assert!(yaml.contains("local_host: \"db.internal\""));
        assert!(!yaml.contains("health_check:"));
    }

    #[test]
    fn yaml_str_escapes() {
        assert_eq!(yaml_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn resolve_cli_path_prefers_env_override() {
        assert_eq!(
            resolve_cli_path_from(Some("/opt/custom/rustunnel")),
            PathBuf::from("/opt/custom/rustunnel")
        );
        // Surrounding whitespace is trimmed off the override.
        assert_eq!(
            resolve_cli_path_from(Some("  /opt/custom/rustunnel ")),
            PathBuf::from("/opt/custom/rustunnel")
        );
        // Whitespace-only / unset → sibling lookup or bare PATH name.
        for empty in [Some("   "), None] {
            let p = resolve_cli_path_from(empty);
            let name = if cfg!(windows) {
                "rustunnel.exe"
            } else {
                "rustunnel"
            };
            assert!(p.ends_with(name));
        }
    }

    #[test]
    fn upgrade_text_rewrites_released_cli_stderr_to_mcp() {
        let raw = "\
some tracing line\n\
error: tunnel error: Hobby plan limit: 2 concurrent tunnels. Pay-as-you-go removes the cap ($3/mo min).\n\
Upgrade: https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=tunnel_limit\n";
        let text = upgrade_text_from_cli_output(raw).unwrap();
        assert!(text.starts_with("Hobby plan limit: 2 concurrent tunnels."));
        assert!(text.contains("utm_source=mcp"));
        assert!(text.contains("utm_content=tunnel_limit"));
        assert!(!text.contains("error: tunnel error:"));
        assert!(!text.contains("utm_source=cli"));
        assert_eq!(cli_exit_tool_error(raw), text);
    }

    #[test]
    fn upgrade_text_keeps_a_new_cli_message_and_rewrites_subdomain() {
        let raw = "\
Hobby plan limit: custom subdomains need Pay-as-you-go ($3/mo min, $0.10/GB).\n\
Upgrade: https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=subdomain\n";
        let text = upgrade_text_from_cli_output(raw).unwrap();
        assert!(text.starts_with("Hobby plan limit: custom subdomains"));
        assert!(text.contains("utm_source=mcp"));
        assert!(text.contains("utm_content=subdomain"));
    }

    #[test]
    fn upgrade_text_ignores_unrelated_cli_errors() {
        assert!(upgrade_text_from_cli_output(
            "error: tunnel error: subdomain 'x' is already in use\n"
        )
        .is_none());
        let msg = cli_exit_tool_error("error: tunnel error: subdomain 'x' is already in use\n");
        assert!(msg.contains("subdomain 'x' is already in use"));
        assert!(msg.contains("exited before"));
    }
}
