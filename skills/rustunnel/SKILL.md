---
name: rustunnel
description: "Expose local services via secure tunnels using rustunnel MCP server. Create public URLs for local HTTP/TCP services for testing, webhooks, and deployment."
version: 1.3.0
author: OpenClaw
tags: [tunnel, ngrok, expose, devops, deployment, testing, webhooks]
---

# Rustunnel - Secure Tunnel Management

Expose local services (HTTP/TCP) through public URLs using rustunnel. Perfect for testing webhooks, sharing local development, and deployment workflows.

## When to Use

- **Webhook testing** - Expose local server to receive webhooks from external services
- **Demo sharing** - Share local development with stakeholders
- **CI/CD integration** - Expose preview environments
- **Database access** - Expose local TCP services (PostgreSQL, Redis, etc.)
- **Mobile testing** - Test mobile apps against local backend

## ⚠️ IMPORTANT: Use MCP Tools, Not CLI

**Always use MCP tools for tunnel management.** They handle lifecycle automatically.

| Method | Lifecycle | Recommended |
|--------|-----------|-------------|
| **MCP tools** (create_tunnel, close_tunnel) | Automatic cleanup | ✅ Yes |
| **CLI** (rustunnel http 3000) | Manual process management | ❌ Only for cloud sandboxes |

**Why MCP tools?**
- Automatic cleanup when closed
- No orphaned processes
- Proper tunnel lifecycle management
- Returns tunnel_id for tracking

---

## Configuration File

**Location:** `~/.rustunnel/config.yml`

This file stores your server address and auth token. The agent will read from this file instead of asking every time.

**Format:**
```yaml
# rustunnel configuration
# Documentation: https://github.com/joaoh82/rustunnel

server: edge.rustunnel.com:4040
auth_token: your-token-here

tunnels:
  expense_tracker:
    proto: http
    local_port: 3000
  # api:
  #   proto: http
  #   local_port: 8080
  #   subdomain: myapi
```

## First-Time Setup

**Before using tunnels, ensure config exists:**

1. **Check if config file exists:** `~/.rustunnel/config.yml`
2. **If not, ask user:** "What's your rustunnel auth token and server address?"
3. **Create config file directly:**
   ```bash
   mkdir -p ~/.rustunnel
   chmod 700 ~/.rustunnel
   ```
4. **Write config with user's token:**
   ```yaml
   server: <user-provided-server>
   auth_token: <user-provided-token>
   ```
5. **Set permissions:** `chmod 600 ~/.rustunnel/config.yml`

---

## Agent Workflow

**Always follow this sequence:**

### Step 1: Check Config

```bash
# Check if config exists
cat ~/.rustunnel/config.yml
```

**If config exists with auth_token:** Read token and proceed.

**If config missing:**
1. Ask user: "What's your rustunnel auth token and server address?"
2. Create config file directly:
   ```bash
   mkdir -p ~/.rustunnel
   chmod 700 ~/.rustunnel
   ```
3. Write config with user's token:
   ```yaml
   server: <user-provided-server>
   auth_token: <user-provided-token>
   ```
4. Set permissions: `chmod 600 ~/.rustunnel/config.yml`

### Step 2: Read Token from Config

When making tool calls, read `auth_token` from `~/.rustunnel/config.yml`:
```yaml
auth_token: your-token-here
server: edge.rustunnel.com:4040
```

Use these values in tool calls - **don't ask the user every time.**

### Step 3: Use MCP Tools

With token from config, call MCP tools directly.

---

## MCP Tools (Recommended)

### create_tunnel

Expose a local port and get a public URL.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token (read from config) |
| `local_port` | integer | yes | Local port to expose |
| `protocol` | "http" \| "tcp" | yes | Tunnel type |
| `subdomain` | string | no | Custom subdomain (HTTP only) |

**Returns:**
```json
{
  "public_url": "https://abc123.edge.rustunnel.com",
  "tunnel_id": "a1b2c3d4-...",
  "protocol": "http"
}
```

**Lifecycle:** Tunnel stays open until `close_tunnel` is called or MCP server exits.

### close_tunnel

Close a tunnel by ID. Public URL stops working immediately.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `tunnel_id` | string | yes | UUID from create_tunnel |

**This is the proper way to close tunnels.** No orphaned processes.

### list_tunnels

List all currently active tunnels.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token (read from config) |

**Returns:** JSON array of tunnel objects.

### get_tunnel_history

Retrieve history of past tunnels.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `protocol` | "http" \| "tcp" | no | Filter by protocol |
| `limit` | integer | no | Max entries (default: 25) |

---

## CLI Fallback (Cloud Sandboxes Only)

Use `get_connection_info` when MCP can't spawn subprocesses (cloud sandboxes, containers).

### get_connection_info

Returns the CLI command string without spawning anything.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `local_port` | integer | yes | Local port to expose |
| `protocol` | "http" \| "tcp" | yes | Tunnel type |

**Returns:**
```json
{
  "cli_command": "rustunnel http 3000 --server edge.rustunnel.com:4040 --token abc123",
  "server": "edge.rustunnel.com:4040",
  "install_url": "https://github.com/joaoh82/rustunnel/releases/latest"
}
```

**Workflow for cloud sandboxes:**
1. Call `get_connection_info()` to get CLI command
2. Output command for user to run locally
3. User runs command in their terminal
4. Call `list_tunnels()` to verify tunnel is active

---

## Common Workflows

### 1. Expose Local API (MCP Tools)

```
1. Read auth_token from ~/.rustunnel/config.yml
2. Create tunnel: create_tunnel(token, local_port=3000, protocol="http")
3. Store tunnel_id for later cleanup
4. Return public_url to user
5. When done: close_tunnel(token, tunnel_id)
```

### 2. Custom Subdomain

```
1. Read auth_token from config
2. create_tunnel(token, local_port=5173, protocol="http", subdomain="myapp-preview")
3. Return URL: https://myapp-preview.edge.rustunnel.com
4. close_tunnel(token, tunnel_id) when done
```

### 3. TCP Tunnel (Database)

```
1. Read auth_token from config
2. create_tunnel(token, local_port=5432, protocol="tcp")
3. Return tcp://host:port for connection
4. close_tunnel(token, tunnel_id) when done
```

### 4. Cloud Sandbox (CLI Fallback)

```
1. Read auth_token from config
2. get_connection_info(token, local_port=3000, protocol="http")
3. Output CLI command for user to run locally
4. User runs command
5. list_tunnels(token) to verify and get public_url
6. When done, user Ctrl+C the CLI process
```

---

## Prerequisites

1. **Rustunnel MCP server installed:**
   ```bash
   # Homebrew (macOS/Linux)
   brew tap joaoh82/rustunnel
   brew install rustunnel
   
   # Or build from source
   git clone https://github.com/joaoh82/rustunnel.git
   cd rustunnel
   make release-mcp
   sudo install -m755 target/release/rustunnel-mcp /usr/local/bin/rustunnel-mcp
   ```

2. **Config file:** `~/.rustunnel/config.yml` with `auth_token` set

## MCP Configuration

Add to your MCP client config:
```json
{
  "mcpServers": {
    "rustunnel": {
      "command": "rustunnel-mcp",
      "args": [
        "--server", "edge.rustunnel.com:4040",
        "--api", "https://edge.rustunnel.com:8443"
      ]
    }
  }
}
```

**Note:** The MCP server address should match the `server` in `~/.rustunnel/config.yml`.

---

## Architecture

```
Internet ──── :443 ────▶ rustunnel-server ────▶ WebSocket ────▶ rustunnel-client ────▶ localhost:PORT
                              │
                        Dashboard (:8443)
                        REST API
```

## Security Notes

- Tokens are sent over HTTPS (use `--insecure` only in local dev)
- MCP tools handle process cleanup automatically
- Tunnels are closed when MCP server exits
- Config file should be protected: `chmod 600 ~/.rustunnel/config.yml`

---

## Related Skills

- **vercel-deploy** - Deploy to Vercel for production hosting
- **here-now** - Instant static file hosting
- **backend** - Backend service patterns
- **nodejs-patterns** - Node.js deployment patterns

## Resources

- [GitHub Repository](https://github.com/joaoh82/rustunnel)
- [MCP Server Documentation](https://github.com/joaoh82/rustunnel/blob/main/docs/mcp-server.md)
- [API Reference](https://github.com/joaoh82/rustunnel/blob/main/docs/api-reference.md)