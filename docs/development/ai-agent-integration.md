# AI Agent Integration Design

## Overview

This document describes a production-ready architecture for making rustunnel
accessible to AI agents and charging them via the x402 micropayment protocol.

It covers:
- The core constraint that shapes the design (tunnels need persistent connections)
- Two deployment modes for the MCP server (local and remote)
- Where x402 fits into the payment flow
- Billing models and what to charge for
- What needs to be built and in what order

---

## The Core Constraint

Tunnels are **not** created via the REST API. They require a persistent TLS
WebSocket connection from a client process to the control port (`4040`).
Traffic is multiplexed over that connection using yamux. When the connection
drops, all tunnels it owns close immediately.

This means an AI agent cannot open a tunnel by calling an HTTP endpoint.
It must either:

1. **Run the `rustunnel` CLI** as a subprocess (the practical path), or
2. **Embed the client library** once a Rust SDK is published.

The design below treats the CLI as the tunnel runtime and places the MCP
server + x402 payment layer on top for discoverability and billing.

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  AI Agent (Claude, GPT-4o, Gemini, custom…)              │
│                                                          │
│  calls MCP tools ──────────────────────────────────────▶ │
│                                                          │
│  ┌──────────────────────────────────────────────────┐   │
│  │  rustunnel MCP Server                            │   │
│  │                                                  │   │
│  │  Tools:                                          │   │
│  │  • purchase_tunnel_pass  ◀── x402 payment here   │   │
│  │  • create_tunnel         ──▶ spawns CLI           │   │
│  │  • list_tunnels          ──▶ REST /api/tunnels    │   │
│  │  • close_tunnel          ──▶ REST /api/tunnels/:id│   │
│  │  • get_connection_info   ──▶ REST /api/status     │   │
│  │  • get_tunnel_history    ──▶ REST /api/history    │   │
│  └──────────────────────────────────────────────────┘   │
│             │                         │                  │
│         CLI subprocess            HTTP REST              │
└───────────── │ ─────────────────────── │ ────────────────┘
               │                         │
               ▼                         ▼
  ┌────────────────────────┐   ┌─────────────────────────┐
  │  rustunnel-server      │   │  rustunnel-server        │
  │  :4040 (control plane) │   │  :8443 (dashboard API)   │
  └────────────────────────┘   └─────────────────────────┘
```

---

## Deployment Modes

### Mode A — Local (stdio transport) — Recommended for most agents

The MCP server runs as a **subprocess on the agent's machine**, communicating
over stdin/stdout. This is the standard MCP deployment for local tools.

```json
// Claude Desktop / any MCP client config
{
  "mcpServers": {
    "rustunnel": {
      "command": "rustunnel-mcp",
      "args": ["--server", "tunnel.rustunnel.com"]
    }
  }
}
```

**Advantages:**
- The MCP server can spawn `rustunnel` CLI directly on the local machine
- Works with any local service the agent is running
- No additional infra beyond the binary

**Limitations:**
- Must be installed on the agent's machine
- The agent's machine must have outbound access to `tunnel.rustunnel.com:4040`

### Mode B — Remote (Streamable HTTP transport) — For cloud agents

The MCP server runs as a hosted service. Because the CLI cannot run on the
remote server and forward traffic to the agent's local machine, this mode
returns **connection instructions** rather than opening tunnels directly.

```
https://mcp.tunnel.rustunnel.com/mcp   ← MCP endpoint
```

The `create_tunnel` tool in this mode returns the CLI command to run locally,
a pre-purchased token, and the expected public URL. The agent runs the command
in its own environment.

This mode is the right choice for agents that:
- Run in cloud sandboxes (e.g., GitHub Actions, Modal, E2B)
- Already have the CLI available in their environment
- Want to manage tokens and monitor tunnels remotely

---

## x402 Payment Integration

### What x402 does

x402 activates the HTTP `402 Payment Required` status code. The flow:

```
Agent ──POST /api/tokens──▶ rustunnel-server
                   ◀── 402  { "accepts": [{ price: "$0.05", network: "base", payTo: "0x..." }] }
Agent pays USDC on-chain
Agent ──POST /api/tokens (+ Payment-Signature header) ──▶ server
                   ◀── 201  { "id": "...", "token": "..." }
```

All payment data travels as base64-encoded JSON in HTTP headers — no body
changes required. The server verifies the payment signature by calling a
**facilitator** (e.g., Coinbase's public facilitator for Base chain) which
handles on-chain verification and settlement.

### Where to gate payment

The single most impactful endpoint to gate is **`POST /api/tokens`**.

A token is the unit of access — it authenticates every tunnel the agent opens.
By charging for token creation you:
- Charge once per "session" or "project" rather than per tunnel
- Keep the WebSocket control plane unchanged (no payment on each tunnel connect)
- Avoid complexity of per-request metering at the tunnel proxy level

Secondary gating options (for future tiers):
| Endpoint | Use case |
|----------|----------|
| `POST /api/tokens` | **Primary gate** — charge for access |
| `GET /api/history` | Premium analytics |
| `GET /api/tunnels/:id/requests` | Request inspection tier |

### Pricing model for agents

| Tier | Price | What it grants |
|------|-------|----------------|
| Micro pass | $0.01 | 1 token, 1 hour TTL, 2 concurrent tunnels |
| Standard pass | $0.10 | 1 token, 24 hour TTL, 10 concurrent tunnels |
| Project pass | $1.00 | 1 token, 30 day TTL, unlimited tunnels |

These map to `scope` and TTL metadata on the token. The server enforces limits
via the existing `max_tunnels_per_session` config extended with per-token TTL
and tunnel count caps.

### x402 server-side implementation

x402 is a thin middleware layer. The Rust server needs to:

1. Before processing `POST /api/tokens`, check for the `Payment-Signature`
   header.
2. If absent, return `402` with the `Payment-Required` header describing the
   price, network, and recipient address.
3. If present, forward the signature to the facilitator's `/verify` endpoint.
4. On verification success, proceed with token creation and return `201`.
5. Optionally call `/settle` in the background to finalize the on-chain
   transfer.

The facilitator handles all blockchain complexity. The server only makes two
HTTP calls.

```rust
// Pseudocode for the middleware
async fn payment_guard(req: Request) -> Result<Request, Response> {
    let sig = req.headers().get("Payment-Signature");
    match sig {
        None => Err(payment_required_response(PRICE_CONFIG)),
        Some(sig) => {
            facilitator::verify(sig, PRICE_CONFIG).await?;
            Ok(req)  // proceed to handler
        }
    }
}
```

**Facilitator:** Use Coinbase's public facilitator for Base/USDC during
development. Switch to a self-hosted facilitator (open source) for production
to avoid the dependency.

---

## MCP Server Tool Definitions

The MCP server is a separate binary (`rustunnel-mcp`) that wraps the REST API
and CLI. Each tool needs a precise natural-language description so the LLM can
reason about when to call it.

### Tool: `purchase_tunnel_pass`

```json
{
  "name": "purchase_tunnel_pass",
  "description": "Purchase an API token (tunnel pass) that grants the ability to create tunnels on rustunnel. Payment is made in USDC stablecoin via the x402 protocol. The raw token is returned only once — store it securely. Use the 'micro' tier for short tasks, 'standard' for a full work session, 'project' for long-running work.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "label": {
        "type": "string",
        "description": "Human-readable name for this token, e.g. 'agent-session-2025-06-01'"
      },
      "tier": {
        "type": "string",
        "enum": ["micro", "standard", "project"],
        "description": "micro=$0.01/1h, standard=$0.10/24h, project=$1.00/30d"
      },
      "wallet_private_key": {
        "type": "string",
        "description": "EVM private key (hex) for the wallet that will pay. Must hold sufficient USDC on Base network."
      }
    },
    "required": ["label", "tier", "wallet_private_key"]
  }
}
```

Returns: `{ token: string, id: string, expires_at: string, tier: string }`

### Tool: `create_tunnel`

```json
{
  "name": "create_tunnel",
  "description": "Open a tunnel to a locally running service and get a public URL. Requires a valid token from purchase_tunnel_pass. The tunnel stays open until close_tunnel is called or the process exits. Use protocol='http' for web services (returns an https:// URL with a subdomain), protocol='tcp' for raw TCP services like databases or SSH (returns a host:port).",
  "inputSchema": {
    "type": "object",
    "properties": {
      "token": {
        "type": "string",
        "description": "API token obtained from purchase_tunnel_pass"
      },
      "local_port": {
        "type": "integer",
        "description": "The local port the service is listening on, e.g. 3000"
      },
      "protocol": {
        "type": "string",
        "enum": ["http", "tcp"],
        "description": "Use 'http' for web/API services, 'tcp' for databases, SSH, or raw TCP"
      },
      "subdomain": {
        "type": "string",
        "description": "Optional custom subdomain for HTTP tunnels. Server assigns a random one if omitted."
      }
    },
    "required": ["token", "local_port", "protocol"]
  }
}
```

Returns: `{ public_url: string, tunnel_id: string, protocol: string }`

### Tool: `list_tunnels`

```json
{
  "name": "list_tunnels",
  "description": "List all tunnels currently open on this server. Returns the public URL, protocol, and traffic count for each active tunnel. Use this to check whether your tunnel is still running or to find the public URL of an existing tunnel.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "token": { "type": "string", "description": "API token" }
    },
    "required": ["token"]
  }
}
```

### Tool: `close_tunnel`

```json
{
  "name": "close_tunnel",
  "description": "Force-close a specific tunnel by its ID. The public URL will stop working immediately. Use list_tunnels to find the tunnel_id.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "token": { "type": "string", "description": "API token" },
      "tunnel_id": { "type": "string", "description": "UUID of the tunnel to close" }
    },
    "required": ["token", "tunnel_id"]
  }
}
```

### Tool: `get_connection_info`

```json
{
  "name": "get_connection_info",
  "description": "Get the server address and CLI command needed to create a tunnel manually. Use this when you want to run the rustunnel CLI yourself rather than using the create_tunnel tool, or when operating in an environment where the MCP server cannot spawn subprocesses.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "token": { "type": "string" },
      "local_port": { "type": "integer" },
      "protocol": { "type": "string", "enum": ["http", "tcp"] }
    },
    "required": ["token", "local_port", "protocol"]
  }
}
```

Returns:
```json
{
  "cli_command": "rustunnel http 3000 --server tunnel.rustunnel.com:4040 --token <token>",
  "server": "tunnel.rustunnel.com:4040",
  "install_url": "https://github.com/joaoh82/rustunnel/releases/latest"
}
```

### Tool: `get_tunnel_history`

```json
{
  "name": "get_tunnel_history",
  "description": "Retrieve the history of past tunnels associated with this server, including their duration and which token opened them. Useful for auditing agent activity or debugging dropped tunnels.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "token": { "type": "string" },
      "protocol": { "type": "string", "enum": ["http", "tcp"], "description": "Filter by protocol. Omit for all." },
      "limit": { "type": "integer", "default": 25 }
    },
    "required": ["token"]
  }
}
```

---

## Agent Workflow Examples

### Workflow A: Agent exposes a local dev server (Mode A — local MCP)

```
Agent: I need to share my local app at port 3000 with a collaborator.

1. Agent calls purchase_tunnel_pass(label="demo", tier="micro", wallet="0x...")
   → MCP server calls POST /api/tokens with x402 payment (USDC on Base)
   → Returns token = "abc123-..."

2. Agent calls create_tunnel(token="abc123-...", local_port=3000, protocol="http")
   → MCP server spawns: rustunnel http 3000 --server tunnel.rustunnel.com:4040 --token abc123-...
   → Returns: { public_url: "https://xyz.tunnel.rustunnel.com", tunnel_id: "..." }

3. Agent returns the public URL to the user. Tunnel stays open in background.

4. Later: Agent calls close_tunnel(tunnel_id="...")
   → MCP server calls DELETE /api/tunnels/:id
   → Tunnel closed, subprocess exits
```

### Workflow B: Agent in a cloud sandbox (Mode B — remote MCP)

```
Agent: Expose my FastAPI app on port 8000.

1. Agent calls purchase_tunnel_pass(tier="standard", ...)
   → x402 payment → Returns token

2. Agent calls get_connection_info(token=..., local_port=8000, protocol="http")
   → Returns: { cli_command: "rustunnel http 8000 --server ... --token ..." }

3. Agent runs the CLI command in its sandbox environment.

4. Agent calls list_tunnels(token=...) after 5 seconds to get the public URL.
```

---

## Implementation Roadmap

### Phase 1 — Minimum viable agent access (no payments yet)

| Item | Description | Effort |
|------|-------------|--------|
| `rustunnel-mcp` binary | MCP server with stdio transport, all tools except payment | Medium |
| Token → CLI bridge | `create_tunnel` tool spawns `rustunnel` subprocess | Small |
| REST tool wrappers | `list_tunnels`, `close_tunnel`, `get_tunnel_history` | Small |
| OpenAPI spec | `GET /openapi.json` on the dashboard port for agent discovery | Small |

Deliverable: Agents can use rustunnel via MCP with a pre-existing token.

### Phase 2 — x402 payment gating

| Item | Description | Effort |
|------|-------------|--------|
| x402 middleware (Rust) | Payment guard on `POST /api/tokens` | Medium |
| Token TTL + tier metadata | Extend tokens table with `expires_at`, `tier`, `tunnel_limit` | Small |
| Token expiry enforcement | Reject tunnel registration if token is expired or over limit | Small |
| `purchase_tunnel_pass` tool | MCP tool that drives x402 payment flow using agent's wallet | Medium |
| Coinbase facilitator integration | Verify + settle calls to `facilitated.x402.org` | Small |

Deliverable: Agents pay USDC to provision tokens. No human intervention needed.

### Phase 3 — Remote MCP server + usage metering

| Item | Description | Effort |
|------|-------------|--------|
| Streamable HTTP transport | MCP server deployed as `mcp.tunnel.rustunnel.com` | Medium |
| OAuth 2.1 on MCP endpoint | Auth for remote MCP access | Medium |
| Usage metering API | `GET /api/usage` returns tunnel-hours, bytes, request counts | Medium |
| Billing webhook | Emit events when token expires or limits hit | Small |

### Phase 4 — Human plan tiers (out of scope for this doc)

Traditional subscription plans for human users sit alongside the per-token
x402 model. The same token infrastructure supports both — the difference is
how the token is provisioned (x402 payment vs. Stripe subscription webhook).

---

## Security Considerations

**Wallet key handling:** The `purchase_tunnel_pass` tool accepts a private key
to sign the payment. In local Mode A, the key stays on the agent's machine.
In remote Mode B, the MCP server never receives a raw private key — instead
the agent signs the x402 payment locally and passes only the
`Payment-Signature` header value.

**Prompt injection:** An adversarial page loaded through an HTTP tunnel could
attempt to inject instructions via the tunnel content. The MCP server should
never expose the raw tunnel traffic to the agent, only metadata.

**Token TTL:** Short-lived tokens (micro/standard tiers) limit blast radius if
a token is leaked. Agents should not store tokens beyond the session.

**Scope enforcement:** Phase 2 adds per-token tunnel limits. Until then, the
existing `max_tunnels_per_session` server config is the backstop.

---

## References

- [x402 protocol spec and SDKs](https://x402.org)
- [x402 GitHub monorepo](https://github.com/coinbase/x402)
- [MCP specification (2025-03-26)](https://spec.modelcontextprotocol.io/specification/)
- [MCP TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk)
- [rustunnel REST API reference](../api-reference.md)
- [rustunnel database schema](../database.md)
