---
name: rustunnel
description: "Expose local services via secure tunnels using rustunnel MCP server. Create public URLs for local HTTP/TCP services for testing, webhooks, and deployment."
version: 0.2.3
author: Joao Henrique Machado Silva
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
  # database:
  #   proto: tcp
  #   local_port: 5432
```

## First-Time Setup

**Before using tunnels, ensure config exists:**

1. **Check if config file exists:** `~/.rustunnel/config.yml`
2. **If not, run setup:**
   ```bash
   rustunnel setup
   ```
   This will prompt for server address and auth token, then create the config file.

3. **Or create manually** with your token from:
   - **Hosted service**: Request via [GitHub Issue](https://github.com/joaoh82/rustunnel/issues/new) titled "Token request"
   - **Self-hosted**: Create via dashboard UI or CLI

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

### Step 3: Use Tools

With token from config, call MCP tools directly.

## Prerequisites

1. **Rustunnel installed:**
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

## Available Tools

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

### list_tunnels

List all currently active tunnels.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token (read from config) |

**Returns:** JSON array of tunnel objects.

### close_tunnel

Force-close a tunnel by ID.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `tunnel_id` | string | yes | UUID from create_tunnel or list_tunnels |

### get_connection_info

Get the CLI command for manual tunnel setup (cloud sandboxes).

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `local_port` | integer | yes | Local port to expose |
| `protocol` | "http" \| "tcp" | yes | Tunnel type |

### get_tunnel_history

Retrieve history of past tunnels.

**Parameters:**
| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `token` | string | yes | API token |
| `protocol` | "http" \| "tcp" | no | Filter by protocol |
| `limit` | integer | no | Max entries (default: 25) |

## Common Workflows

### 1. First-Time Setup

```
1. Check if ~/.rustunnel/config.yml exists
2. If not, ask user for auth_token and server
3. Create directory: mkdir -p ~/.rustunnel && chmod 700 ~/.rustunnel
4. Write config file with user's credentials
5. Set permissions: chmod 600 ~/.rustunnel/config.yml
6. Ready to use tunnels
```

### 2. Expose Local API

```
1. Read auth_token from ~/.rustunnel/config.yml
2. Create tunnel: create_tunnel(token, local_port=3000, protocol="http")
3. Return public_url to user
4. When done: close_tunnel(token, tunnel_id)
```

### 3. Named Tunnels (Persistent Config)

```
1. Add tunnel to ~/.rustunnel/config.yml:
   tunnels:
     myapp:
       proto: http
       local_port: 3000
       subdomain: myapp-preview

2. Start named tunnel: rustunnel start myapp
   Or use MCP: create_tunnel with same settings
```

### 4. Cloud Sandbox (No Subprocess)

```
1. Read auth_token from config
2. get_connection_info(token, local_port=3000, protocol="http")
3. Output CLI command for user to run locally
4. Verify: list_tunnels(token)
```

## Architecture

```
Internet ──── :443 ────▶ rustunnel-server ────▶ WebSocket ────▶ rustunnel-client ────▶ localhost:PORT
                              │
                        Dashboard (:8443)
                        REST API
```

## Security Notes

- Tokens are sent over HTTPS (use `--insecure` only in local dev)
- Child processes are killed when MCP server exits
- Tunnels don't persist across MCP server restarts
- Config file should be protected: `chmod 600 ~/.rustunnel/config.yml`

## Related Skills

- **vercel-deploy** - Deploy to Vercel for production hosting
- **here-now** - Instant static file hosting
- **backend** - Backend service patterns
- **nodejs-patterns** - Node.js deployment patterns

## Resources

- [GitHub Repository](https://github.com/joaoh82/rustunnel)
- [MCP Server Documentation](https://github.com/joaoh82/rustunnel/blob/main/docs/mcp-server.md)
- [API Reference](https://github.com/joaoh82/rustunnel/blob/main/docs/api-reference.md)