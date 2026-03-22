//! Tool definitions, input schemas, and execution logic for all Phase-1 tools.
//!
//! Tools implemented here:
//!   - `create_tunnel`    — spawns `rustunnel` CLI and polls API for the URL
//!   - `list_tunnels`     — GET /api/tunnels wrapper
//!   - `close_tunnel`     — DELETE /api/tunnels/:id + kills spawned process
//!   - `get_connection_info` — returns CLI command string (no API call)
//!   - `get_tunnel_history`  — GET /api/history wrapper

use std::collections::HashSet;
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

// ── tool definitions (returned by tools/list) ─────────────────────────────────

pub fn tool_definitions() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "create_tunnel",
            "description": "Open a tunnel to a locally running service and get a public URL. \
                Requires a valid API token. Spawns the rustunnel CLI as a subprocess — the \
                tunnel stays open until close_tunnel is called or the MCP server exits. \
                Use protocol='http' for web services (returns an https:// URL), \
                protocol='tcp' for databases, SSH, or raw TCP (returns a host:port).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "API token for authentication"
                    },
                    "local_port": {
                        "type": "integer",
                        "description": "Local port the service is listening on, e.g. 3000"
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["http", "tcp"],
                        "description": "Use 'http' for web/API services, 'tcp' for databases or SSH"
                    },
                    "subdomain": {
                        "type": "string",
                        "description": "Optional custom subdomain for HTTP tunnels. \
                            Server assigns a random one if omitted."
                    },
                    "region": {
                        "type": "string",
                        "description": "Optional region ID (e.g. 'eu', 'us', 'ap'). \
                            Omit to let the client auto-select the nearest region by latency. \
                            Use list_regions to see available regions."
                    }
                },
                "required": ["token", "local_port", "protocol"]
            }
        }),
        serde_json::json!({
            "name": "list_tunnels",
            "description": "List all tunnels currently open on this server. Returns the public \
                URL, protocol, and traffic count for each active tunnel. Use this to check \
                whether your tunnel is still running or to find the public URL of an existing tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "API token for authentication"
                    }
                },
                "required": ["token"]
            }
        }),
        serde_json::json!({
            "name": "close_tunnel",
            "description": "Force-close a specific tunnel by its ID. The public URL stops \
                working immediately. Use list_tunnels to find the tunnel_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "API token for authentication"
                    },
                    "tunnel_id": {
                        "type": "string",
                        "description": "UUID of the tunnel to close"
                    }
                },
                "required": ["token", "tunnel_id"]
            }
        }),
        serde_json::json!({
            "name": "get_connection_info",
            "description": "Get the CLI command needed to create a tunnel manually. Use this \
                when the MCP server cannot spawn subprocesses (e.g. cloud sandbox) or when \
                you prefer to run the client yourself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "API token for authentication"
                    },
                    "local_port": {
                        "type": "integer",
                        "description": "Local port the service is listening on"
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["http", "tcp"],
                        "description": "Tunnel protocol"
                    },
                    "region": {
                        "type": "string",
                        "description": "Optional region ID (e.g. 'eu', 'us', 'ap'). \
                            Omit to let the client auto-select."
                    }
                },
                "required": ["token", "local_port", "protocol"]
            }
        }),
        serde_json::json!({
            "name": "list_regions",
            "description": "List available tunnel server regions with their IDs, names, \
                and locations. Use this to pick a specific region for create_tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "get_tunnel_history",
            "description": "Retrieve the history of past tunnels, including their duration and \
                which token opened them. Useful for auditing activity or debugging dropped tunnels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "API token for authentication"
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["http", "tcp"],
                        "description": "Optional: filter by protocol. Omit for all."
                    },
                    "limit": {
                        "type": "integer",
                        "default": 25,
                        "description": "Maximum number of entries to return"
                    }
                },
                "required": ["token"]
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

// ── tool implementations ──────────────────────────────────────────────────────

async fn create_tunnel(args: &Value, state: &Arc<State>) -> Value {
    let token = req_str!(args, "token");
    let protocol = req_str!(args, "protocol");

    let local_port = match args.get("local_port").and_then(Value::as_u64) {
        Some(p) => p,
        None => return tool_err("missing required argument: local_port"),
    };

    let subdomain = args.get("subdomain").and_then(Value::as_str);
    let region = args.get("region").and_then(Value::as_str);

    // Snapshot existing tunnel IDs before spawning so we can identify the new one.
    let before_ids: HashSet<String> = match state.api.list_tunnels(token).await {
        Ok(ts) => ts.iter().map(|t| t.tunnel_id.clone()).collect(),
        Err(e) => return tool_err(format!("failed to query current tunnels: {e}")),
    };

    // Build the CLI command.
    let mut cmd = tokio::process::Command::new("rustunnel");
    cmd.arg(protocol)
        .arg(local_port.to_string())
        .arg("--server")
        .arg(&state.server_addr)
        .arg("--token")
        .arg(token)
        // Suppress CLI output so it doesn't interfere with MCP stdio.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());

    if state.insecure {
        cmd.arg("--insecure");
    }

    if let Some(sub) = subdomain {
        cmd.arg("--subdomain").arg(sub);
    }

    if let Some(r) = region {
        cmd.arg("--region").arg(r);
    }

    let mut child_opt = match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            return tool_err(format!(
                "failed to spawn rustunnel: {e}. \
                Is the 'rustunnel' binary installed and in PATH?"
            ))
        }
    };

    // Poll the API for up to 15 seconds waiting for the new tunnel to appear.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;

        match state.api.list_tunnels(token).await {
            Ok(tunnels) => {
                let new = tunnels
                    .into_iter()
                    .find(|t| !before_ids.contains(&t.tunnel_id));

                if let Some(tunnel) = new {
                    let tunnel_id = tunnel.tunnel_id.clone();
                    state
                        .tunnel_manager
                        .insert(tunnel_id.clone(), child_opt.take().unwrap())
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

    // Timeout — kill the subprocess to avoid a leaked process.
    if let Some(mut child) = child_opt {
        let _ = child.start_kill();
    }

    tool_err(
        "timeout: tunnel did not appear within 15 seconds. \
        Check that the server address and token are correct.",
    )
}

async fn list_tunnels(args: &Value, state: &Arc<State>) -> Value {
    let token = req_str!(args, "token");

    match state.api.list_tunnels(token).await {
        Ok(tunnels) => tool_ok(serde_json::to_string_pretty(&tunnels).unwrap()),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

async fn close_tunnel(args: &Value, state: &Arc<State>) -> Value {
    let token = req_str!(args, "token");
    let tunnel_id = req_str!(args, "tunnel_id");

    // Kill the spawned process if we were the ones that opened this tunnel.
    state.tunnel_manager.kill(tunnel_id).await;

    // Close via the REST API (authoritative close — removes it from routing).
    match state.api.close_tunnel(token, tunnel_id).await {
        Ok(204) | Ok(200) => tool_ok("Tunnel closed successfully."),
        Ok(404) => tool_err("Tunnel not found. It may have already been closed."),
        Ok(401) => tool_err("Authentication failed — check your token."),
        Ok(status) => tool_err(format!("Unexpected status from server: {status}")),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}

fn get_connection_info(args: &Value, state: &Arc<State>) -> Value {
    let token = req_str!(args, "token");
    let protocol = req_str!(args, "protocol");
    let region = args.get("region").and_then(Value::as_str);

    let local_port = match args.get("local_port").and_then(Value::as_u64) {
        Some(p) => p,
        None => return tool_err("missing required argument: local_port"),
    };

    let mut cmd = format!(
        "rustunnel {protocol} {local_port} --server {} --token {token}",
        state.server_addr
    );
    if let Some(r) = region {
        cmd.push_str(&format!(" --region {r}"));
    }
    if state.insecure {
        cmd.push_str(" --insecure");
    }

    let mut info = serde_json::json!({
        "cli_command":  cmd,
        "server":       state.server_addr,
        "install_url":  "https://github.com/joaoh82/rustunnel/releases/latest"
    });
    if let Some(r) = region {
        info["region"] = serde_json::Value::String(r.to_string());
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
    let token = req_str!(args, "token");
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(25);
    let protocol = args.get("protocol").and_then(Value::as_str);

    match state.api.get_history(token, limit, protocol).await {
        Ok(history) => tool_ok(serde_json::to_string_pretty(&history).unwrap()),
        Err(e) => tool_err(format!("API error: {e}")),
    }
}
