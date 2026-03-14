# rustunnel Client — User Guide

`rustunnel` exposes local services to the internet through a secure, self-hosted tunnel server.

---

## Table of Contents

1. [Installation](#installation)
2. [Quick Start](#quick-start)
3. [Configuration File](#configuration-file)
4. [Commands](#commands)
   - [setup — Interactive config wizard](#setup--interactive-config-wizard)
   - [http — HTTP tunnel](#http--http-tunnel)
   - [tcp — TCP tunnel](#tcp--tcp-tunnel)
   - [start — Multi-tunnel mode](#start--multi-tunnel-mode)
   - [token create — API token management](#token-create--api-token-management)
5. [Flags Reference](#flags-reference)
6. [Reconnection Behavior](#reconnection-behavior)
7. [Terminal Output](#terminal-output)
8. [Environment Variables](#environment-variables)
9. [Error Reference](#error-reference)
10. [Troubleshooting](#troubleshooting)

---

## Installation

### From source (recommended)

Requires [Rust](https://rustup.rs/) 1.75 or later.

```bash
git clone https://github.com/your-org/rustunnel
cd rustunnel
cargo build --release -p rustunnel-client
sudo install -Dm755 target/release/rustunnel /usr/local/bin/rustunnel
```

Or use the Makefile shortcut:

```bash
make deploy-client
```

### Verify

```bash
rustunnel --version
```

---

## Quick Start

```bash
# 1. Create a config file interactively
rustunnel setup
# → prompts for server address and auth token, writes ~/.rustunnel/config.yml

# 2. Expose a local web server running on port 3000
rustunnel http 3000

# 3. Expose a raw TCP service (e.g. SSH on port 22)
rustunnel tcp 22
```

After connecting, the terminal displays the public URL:

```
╭────────────────────────────────────────────────────────────╮
│                         rustunnel                          │
├────────────────────────────────────────────────────────────┤
│  HTTP [myapp] → localhost:3000                             │
│   https://myapp.tunnel.example.com                        │
╰────────────────────────────────────────────────────────────╯

  ✓ Tunnels active. Press Ctrl-C to quit.
```

---

## Configuration File

The client reads `~/.rustunnel/config.yml` automatically. CLI flags always override file values.

### Full example

```yaml
# Tunnel server address (required)
server: tunnel.example.com:9000

# Authentication token (required)
auth_token: rt_live_abc123...

# Skip TLS certificate verification — local dev ONLY, never use in production
insecure: false

# Named tunnel definitions used by `rustunnel start`
tunnels:
  web:
    proto: http
    local_port: 3000
    local_host: localhost       # optional, defaults to localhost
    subdomain: myapp            # optional, requests a specific subdomain

  api:
    proto: http
    local_port: 8080
    subdomain: myapi

  database:
    proto: tcp
    local_port: 5432
```

### Field reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | string | — | Tunnel server host:port (e.g. `tunnel.example.com:9000`) |
| `auth_token` | string | — | Authentication token issued by the server |
| `insecure` | bool | `false` | Skip TLS certificate verification (dev only) |
| `tunnels` | map | `{}` | Named tunnel definitions (used by `rustunnel start`) |
| `tunnels.<name>.proto` | string | — | `http` or `tcp` |
| `tunnels.<name>.local_port` | integer | — | Local port to forward |
| `tunnels.<name>.local_host` | string | `localhost` | Local hostname to connect to |
| `tunnels.<name>.subdomain` | string | auto-assigned | Requested HTTP subdomain |

---

## Commands

### `setup` — Interactive config wizard

Create (or overwrite) `~/.rustunnel/config.yml` through a guided prompt sequence.

```
rustunnel setup
```

**Prompts:**

| Prompt | Default | Description |
|--------|---------|-------------|
| Server address | `edge.rustunnel.com:4040` | The control-plane host:port to connect to |
| Auth token | _(blank)_ | Token issued by the server; leave empty to fill in later |

**Behaviour:**

- Creates `~/.rustunnel/` if the directory doesn't exist.
- If a config file already exists it is overwritten — a backup is not kept, so copy the old file first if you want to preserve it.
- Writes a commented `tunnels:` block with HTTP and TCP examples so you can see the structure right away.
- Prints `Created:` or `Updated:` with the full path when done.

**Example session:**

```
rustunnel setup — create ~/.rustunnel/config.yml

Tunnel server address [edge.rustunnel.com:4040]:
Auth token (leave blank to skip): rt_live_abc123xyz

Created: /Users/alice/.rustunnel/config.yml
Run `rustunnel start` to connect using this config.
```

**Generated file:**

```yaml
# rustunnel configuration
# Documentation: https://github.com/joaoh82/rustunnel

server: edge.rustunnel.com:4040
auth_token: rt_live_abc123xyz

# tunnels:
#   web:
#     proto: http
#     local_port: 3000
#   api:
#     proto: http
#     local_port: 8080
#     subdomain: myapi
#   database:
#     proto: tcp
#     local_port: 5432
```

After running `setup`, uncomment and fill in the `tunnels:` section then run `rustunnel start`, or use `rustunnel http <port>` / `rustunnel tcp <port>` directly.

---

### `http` — HTTP tunnel

Expose a local HTTP/HTTPS service through the tunnel server.

```
rustunnel http <port> [options]
```

**Arguments:**

| Argument | Description |
|----------|-------------|
| `<port>` | Local TCP port to forward (e.g. `3000`) |

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--subdomain <name>` | auto-assigned | Request a specific subdomain (e.g. `myapp` → `myapp.tunnel.example.com`) |
| `--server <host:port>` | from config | Override the server address |
| `--token <token>` | from config | Override the auth token |
| `--local-host <host>` | `localhost` | Local hostname to forward to |
| `--no-reconnect` | off | Exit instead of reconnecting on failure |
| `--insecure` | off | Skip TLS verification (dev only) |

**Examples:**

```bash
# Expose port 3000 with an auto-assigned subdomain
rustunnel http 3000

# Request a specific subdomain
rustunnel http 3000 --subdomain myapp

# Forward to a non-localhost service
rustunnel http 8080 --local-host 192.168.1.10

# One-shot connection (exit on disconnect instead of reconnecting)
rustunnel http 3000 --no-reconnect

# Use a different server and token without a config file
rustunnel http 3000 --server tunnel.example.com:9000 --token rt_live_abc123
```

---

### `tcp` — TCP tunnel

Expose any raw TCP service (database, SSH, game server, etc.).

```
rustunnel tcp <port> [options]
```

**Arguments:**

| Argument | Description |
|----------|-------------|
| `<port>` | Local TCP port to forward |

**Options:** Same as `http` except `--subdomain` has no effect for TCP tunnels.

**Examples:**

```bash
# Expose a local PostgreSQL instance
rustunnel tcp 5432

# Expose SSH on a non-standard port
rustunnel tcp 2222 --local-host 10.0.0.5
```

The server assigns a random public port from its configured TCP port range. The public address is displayed in the startup box.

---

### `start` — Multi-tunnel mode

Start all tunnels defined in a config file simultaneously.

```
rustunnel start [--config <path>]
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-c, --config <path>` | `~/.rustunnel/config.yml` | Path to a config file |

**Example:**

```bash
# Use default config file
rustunnel start

# Use a custom config file
rustunnel start --config /etc/rustunnel/production.yml
```

`start` always reconnects automatically (equivalent to running each tunnel without `--no-reconnect`). At least one tunnel must be defined in the config file or the command exits with an error.

---

### `token create` — API token management

Create a new API token via the server's dashboard REST API. Requires admin credentials.

```
rustunnel token create --name <label> [options]
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--name <label>` | — | Human-readable label for the token (required) |
| `--server <host:port>` | `localhost:4040` | Dashboard server address |
| `--admin-token <token>` | — | Admin token for authentication |

**Example:**

```bash
rustunnel token create \
  --name "production-server" \
  --server tunnel.example.com:4040 \
  --admin-token admin_secret_here
```

**Output:**

```
Token created:
  id:    f47ac10b-58cc-4372-a567-0e02b2c3d479
  token: rt_live_abc123xyz...
  label: production-server
```

Copy the `token` value — it is shown only once. Add it to your config file as `auth_token`.

---

## Flags Reference

This table summarises all flags across all commands:

| Flag | Commands | Description |
|------|----------|-------------|
| `--server <host:port>` | http, tcp | Tunnel server address |
| `--token <token>` | http, tcp | Auth token (overrides config) |
| `--subdomain <name>` | http | Requested HTTP subdomain |
| `--local-host <host>` | http, tcp | Local hostname (default: `localhost`) |
| `--no-reconnect` | http, tcp | Exit on failure instead of reconnecting |
| `--insecure` | http, tcp | Skip TLS certificate verification |
| `-c, --config <path>` | start | Config file path |
| `--name <label>` | token create | Token label (required) |
| `--admin-token <token>` | token create | Admin token for dashboard API |
| `--version` | all | Print version and exit |
| `--help` | all | Print help and exit |

`setup` takes no flags — all input is collected interactively.

---

## Reconnection Behavior

By default, `rustunnel` reconnects automatically when the connection drops. The retry delay follows an **exponential backoff** schedule:

| Attempt | Delay |
|---------|-------|
| 1 | ~1 s |
| 2 | ~2 s |
| 3 | ~4 s |
| 4 | ~8 s |
| … | … |
| n≥6 | ~60 s (max) |

Each delay has ±20% random jitter to prevent thundering-herd reconnects when a server restarts.

```
  Reconnecting in 2.3s (attempt 2)…
  Reconnecting in 5.1s (attempt 3)…
```

### Fatal errors (no reconnect)

The following errors cause an immediate exit — retrying would not help:

- **Auth failed** — invalid or revoked token. Fix: run `rustunnel token create` and update your config.
- **Config error** — missing required fields. Fix: check your `~/.rustunnel/config.yml`.

### Disabling reconnect

Use `--no-reconnect` for scripting, CI, or when you want manual control:

```bash
rustunnel http 3000 --no-reconnect || echo "Tunnel exited"
```

---

## Terminal Output

### Connecting spinner

While establishing the connection a spinner is shown:

```
⠙ Connecting to tunnel server…
⠹ Authenticating…
⠸ Registering tunnels…
```

### Startup box

Once all tunnels are registered, a bordered box appears:

```
╭────────────────────────────────────────────────────────────╮
│                         rustunnel                          │
├────────────────────────────────────────────────────────────┤
│   HTTP [myapp] → localhost:3000                            │
│   https://myapp.tunnel.example.com                        │
│   TCP  [ssh]   → localhost:22                             │
│   tcp://tunnel.example.com:34521                          │
╰────────────────────────────────────────────────────────────╯

  ✓ Tunnels active. Press Ctrl-C to quit.
```

Color coding:
- Protocol label — **bold yellow**
- Tunnel name — dim
- Public URL — **bold green**
- Border — cyan

### Graceful shutdown

Press `Ctrl-C` to cleanly close the tunnel and exit. The control WebSocket is closed before the process exits.

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level filter (e.g. `debug`, `info`, `warn`, `rustunnel=debug`). Default: `warn`. |

**Examples:**

```bash
# Enable debug logging for all crates
RUST_LOG=debug rustunnel http 3000

# Enable debug only for rustunnel internals
RUST_LOG=rustunnel=debug rustunnel http 3000

# Quiet mode (errors only)
RUST_LOG=error rustunnel http 3000
```

Log output goes to **stderr**. Normal tunnel output (startup box, reconnect messages) goes to **stdout**.

---

## Error Reference

| Error | Cause | Fix |
|-------|-------|-----|
| `config error: server address is required` | No `--server` flag and no config file | Add `server:` to `~/.rustunnel/config.yml` or pass `--server` |
| `auth failed: <message>` | Token invalid or revoked | Create a new token with `rustunnel token create` |
| `tunnel error: <message>` | Subdomain already in use or server limit reached | Use a different `--subdomain` or wait |
| `connection error: control WS: …` | Can't reach the server | Check network, firewall, and server address |
| `connection error: heartbeat timeout` | Server stopped responding to pings | Transient — reconnect loop will retry |
| `connection error: timeout waiting for server response` | Auth/registration timed out (10 s) | Check server health; may be overloaded |
| `no tunnels defined in config file` | `rustunnel start` with an empty `tunnels:` map | Add at least one tunnel to the config |

---

## Troubleshooting

### Tunnel connects but requests don't arrive

- Verify your local service is running and listening: `curl http://localhost:<port>`
- Check `--local-host` if forwarding to a non-localhost address

### Certificate verification failed

If your server uses a self-signed certificate (common for local/staging environments), use `--insecure`:

```bash
rustunnel http 3000 --insecure
```

**Never use `--insecure` in production** — it disables all TLS certificate checks.

### Subdomain already taken

The server returns `tunnel error: subdomain already in use`. Either:
- Omit `--subdomain` to get an auto-assigned subdomain, or
- Choose a different name: `--subdomain myapp-dev`

### Debugging connection issues

Enable verbose logging to see full protocol traces:

```bash
RUST_LOG=debug rustunnel http 3000 2>&1 | tee rustunnel.log
```

Key log messages to look for:

| Message | Meaning |
|---------|---------|
| `authenticated session_id=...` | Auth succeeded |
| `tunnel registered public_url=...` | Tunnel is active |
| `data WebSocket connected` | Data plane is ready |
| `new connection from server conn_id=...` | Incoming proxied request |
| `yamux data conn error` | Data-plane transport error |
| `heartbeat timeout` | Server stopped responding — will reconnect |

### Multiple tunnels on the same server

Use `rustunnel start` with a config file to open all tunnels over a single control connection:

```yaml
server: tunnel.example.com:9000
auth_token: rt_live_abc123

tunnels:
  frontend:
    proto: http
    local_port: 3000
    subdomain: app
  backend:
    proto: http
    local_port: 8080
    subdomain: api
  metrics:
    proto: tcp
    local_port: 9090
```

```bash
rustunnel start
```
