# rustunnel

[![CI](https://github.com/joaoh82/rustunnel/actions/workflows/ci.yml/badge.svg)](https://github.com/joaoh82/rustunnel/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.76%2B-orange.svg)](https://www.rust-lang.org)

A ngrok-style secure tunnel server written in Rust. Expose local services through a public server over encrypted WebSocket connections with TLS termination, HTTP/TCP proxying, a live dashboard, Prometheus metrics, and audit logging.

You can self-host or use our managed service.

---

## Table of Contents

- [Hosted service](#hosted-service)
- [Architecture overview](#architecture-overview)
- [Requirements](#requirements)
- [Local development setup](#local-development-setup)
  - [Build](#build)
  - [Run tests](#run-tests)
  - [Run the server locally](#run-the-server-locally)
  - [Run the client locally](#run-the-client-locally)
  - [Git hooks](#git-hooks)
- [Production deployment (Ubuntu / systemd)](#production-deployment-ubuntu--systemd)
  - [1 — Install dependencies](#1--install-dependencies)
  - [2 — Build release binaries](#2--build-release-binaries)
  - [3 — Create system user and directories](#3--create-system-user-and-directories)
  - [4 — Install the server binary](#4--install-the-server-binary)
  - [5 — Create the server config file](#5--create-the-server-config-file)
  - [6 — TLS certificates (Let's Encrypt + Cloudflare)](#6--tls-certificates-lets-encrypt--cloudflare)
  - [7 — Set up systemd service](#7--set-up-systemd-service)
  - [8 — Open firewall ports](#8--open-firewall-ports)
  - [9 — Verify the server is running](#9--verify-the-server-is-running)
  - [Updating the server](#updating-the-server)
- [Docker deployment](#docker-deployment) · [full guide](docs/docker-deployment.md)
- [Client configuration](#client-configuration)
  - [Installation](#installation)
  - [Setup wizard](#setup-wizard)
  - [Quick start (CLI flags)](#quick-start-cli-flags)
  - [Config file](#config-file)
  - [Token management](#token-management)
- [Port reference](#port-reference)
- [Config file reference (server)](#config-file-reference-server)
- [REST API](#rest-api)
- [AI agent integration (MCP server)](#ai-agent-integration-mcp-server)
  - [OpenClaw skill](#openclaw-skill)
- [Monitoring](#monitoring)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)
- [Contact](#contact)

---

## Hosted service

You can use rustunnel without running your own server. We operate a public edge server at **`edge.rustunnel.com`** that you can connect to immediately.

### Available servers

| Server | Region | Control plane | Status |
|--------|--------|---------------|--------|
| `edge.rustunnel.com` | Europe (Helsinki) | `:4040` | Live |

More regions are coming — follow the project for updates.

### Getting an auth token

Access to the hosted service requires an auth token. To request one:

1. Open a [GitHub Issue](https://github.com/joaoh82/rustunnel/issues/new) titled **"Token request"**
2. Include your **email address** or **Discord username** in the body
3. We will send you a token privately

Tokens are issued manually for now while the service is in early access.

### Quick start with the hosted server

Once you have a token, run the setup wizard:

```bash
rustunnel setup
# Server address [edge.rustunnel.com:4040]: (press Enter)
# Auth token: <paste your token>
```

Then expose a local service:

```bash
# HTTP tunnel — get a public HTTPS URL for your local port 3000
rustunnel http 3000

# Custom subdomain
rustunnel http 3000 --subdomain myapp

# TCP tunnel — e.g. expose a local database
rustunnel tcp 5432
```

The client prints the public URL as soon as the tunnel is established:

```
✓ tunnel open  https://abc123.edge.rustunnel.com
```

---

## Architecture overview

```
                        ┌──────────────────────────────────────────┐
                        │           rustunnel-server               │
                        │                                          │
Internet ──── :80 ─────▶│  HTTP edge (301 → HTTPS)                 │
Internet ──── :443 ────▶│  HTTPS edge  ──▶ yamux stream ──▶ client │
Client ───── :4040 ────▶│  Control-plane WebSocket (TLS)           │
Browser ──── :8443 ────▶│  Dashboard UI + REST API                 │
Prometheus ─ :9090 ────▶│  Metrics endpoint                        │
Internet ── :20000+ ───▶│  TCP tunnel ports (one per TCP tunnel)   │
                        └──────────────────────────────────────────┘
                                          │ yamux multiplexed streams
                                          ▼
                              ┌─────────────────────┐
                              │   rustunnel client   │
                              │  (developer laptop)  │
                              └──────────┬──────────┘
                                         │ localhost
                                         ▼
                                ┌────────────────┐
                                │  local service  │
                                │  e.g. :3000    │
                                └────────────────┘
```

---

## Requirements

### To build

| Requirement | Version | Notes |
|---|---|---|
| Rust toolchain | 1.76+ | Install via [rustup](https://rustup.rs) |
| `pkg-config` | any | Needed by `reqwest` (TLS) |
| `libssl-dev` | any | On Debian/Ubuntu: `apt install libssl-dev` |
| Node.js + npm | 18+ | Only needed to rebuild the dashboard UI |

### To run the server in production

| Requirement | Notes |
|---|---|
| Linux (Ubuntu 22.04+) | systemd service included |
| TLS certificate + private key | PEM format (Let's Encrypt recommended) |
| Public IP / DNS | Wildcard DNS `*.tunnel.yourdomain.com → server IP` required for HTTP tunnels |

---

## Local development setup

### Build

```bash
# Clone the repository
git clone https://github.com/joaoh82/rustunnel.git
cd rustunnel

# Compile all workspace crates (debug mode)
cargo build --workspace

# Or use the Makefile shortcut
make build
```

### Run tests

The integration test suite spins up a real server on random ports and exercises auth, HTTP tunnels, TCP tunnels, and reconnection logic. It requires a running PostgreSQL instance.

```bash
# Start the local PostgreSQL container (once per machine, persists across reboots)
make db-start

# Full suite (unit + integration)
make test

# With output visible
TEST_DATABASE_URL=postgres://rustunnel:test@localhost:5432/rustunnel_test \
  cargo test --workspace -- --nocapture

# Stop PostgreSQL when you no longer need it
make db-stop
```

`make db-start` runs `deploy/docker-compose.dev-deps.yml` which starts a Postgres 16 container on `localhost:5432`. The `make test` target injects `TEST_DATABASE_URL` automatically. If you run `cargo test` directly, export the variable first:

```bash
export TEST_DATABASE_URL=postgres://rustunnel:test@localhost:5432/rustunnel_test
```

### Run the server locally

Generate a self-signed certificate for local testing:

```bash
mkdir -p /tmp/rustunnel-dev

openssl req -x509 -newkey rsa:2048 -keyout /tmp/rustunnel-dev/key.pem \
  -out /tmp/rustunnel-dev/cert.pem -days 365 -nodes \
  -subj "/CN=localhost"
```

A ready-made local config is checked into the repository at **`deploy/local/server.toml`**.
It points to the self-signed cert paths above and has auth disabled for convenience.
Start the server with it directly:

```bash
cargo run -p rustunnel-server -- --config deploy/local/server.toml
```

Key settings in `deploy/local/server.toml`:

| Setting | Value |
|---|---|
| Domain | `localhost` |
| HTTP edge | `:8080` |
| HTTPS edge | `:8443` |
| Control plane | `:4040` |
| Dashboard | `:4041` |
| Auth token | `dev-secret-change-me` |
| Auth required | `false` |
| TLS cert | `/tmp/rustunnel-dev/cert.pem` |
| TLS key | `/tmp/rustunnel-dev/key.pem` |
| Database | `/tmp/rustunnel-dev/rustunnel.db` |

### Run the client locally

With the server running, expose a local service (e.g. something on port 3000):

```bash
# HTTP tunnel
cargo run -p rustunnel-client -- http 3000 \
  --server localhost:4040 \
  --token dev-secret-change-me \
  --insecure

# TCP tunnel
cargo run -p rustunnel-client -- tcp 5432 \
  --server localhost:4040 \
  --token dev-secret-change-me \
  --insecure
```

> `--insecure` skips TLS certificate verification. Required when using a
> self-signed certificate locally. Never use this flag against a production server.

The client will print a public URL, for example:
```
http tunnel  →  http://abc123.localhost:8080
tcp  tunnel  →  tcp://localhost:20000
```

### Testing the HTTP tunnel locally

The tunnel URL uses a subdomain (e.g. `http://abc123.localhost:8080`).
Browsers won't resolve `*.localhost` subdomains by default, so you have two options:

**Option A — curl with a Host header (no setup required)**

```bash
curl -v -H "Host: abc123.localhost" http://localhost:8080/
```

**Option B — wildcard DNS via dnsmasq (enables browser access)**

```bash
# Install and configure dnsmasq to resolve *.localhost → 127.0.0.1
brew install dnsmasq
echo "address=/.localhost/127.0.0.1" | sudo tee -a $(brew --prefix)/etc/dnsmasq.conf
sudo brew services start dnsmasq

# Tell macOS to use dnsmasq for .localhost queries
sudo mkdir -p /etc/resolver
echo "nameserver 127.0.0.1" | sudo tee /etc/resolver/localhost
```

Then visit `http://abc123.localhost:8080` in the browser (include `:8080` since the
local config uses port 8080, not port 80).

### Git hooks

A pre-push hook is included in `.githooks/` that mirrors the CI check step
(format check + Clippy). Run this once after cloning to activate it:

```bash
make install-hooks
```

From that point on, every `git push` will automatically run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

If either check fails the push is aborted, keeping the remote branch green.

---

## Production deployment (Ubuntu / systemd)

The steps below match a deployment where:
- Domain: `edge.rustunnel.com`
- Wildcard DNS: `*.edge.rustunnel.com → <server IP>`
- TLS certs: Let's Encrypt via Certbot + Cloudflare DNS challenge

### 1 — Install dependencies

```bash
apt update && apt install -y \
  pkg-config libssl-dev curl git \
  certbot python3-certbot-dns-cloudflare
```

Install Rust (as the build user, not root):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 2 — Build release binaries

```bash
git clone https://github.com/joaoh82/rustunnel.git
cd rustunnel
cargo build --release -p rustunnel-server -p rustunnel-client
```

Binaries will be at:
- `target/release/rustunnel-server`
- `target/release/rustunnel`

### 3 — Create system user and directories

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin rustunnel

mkdir -p /etc/rustunnel /var/lib/rustunnel
chown rustunnel:rustunnel /var/lib/rustunnel
chmod 750 /var/lib/rustunnel
```

### 4 — Install the server binary

```bash
install -Dm755 target/release/rustunnel-server /usr/local/bin/rustunnel-server

# Optionally install the client system-wide
install -Dm755 target/release/rustunnel /usr/local/bin/rustunnel
```

Or use the Makefile target (runs build + install + systemd setup):

```bash
sudo make deploy
```

### 5 — Create the server config file

Create `/etc/rustunnel/server.toml` with the content below.
Replace `your-admin-token-here` with a strong random secret (e.g. `openssl rand -hex 32`).

```toml
# /etc/rustunnel/server.toml

[server]
# Primary domain — must match your wildcard DNS record.
domain       = "edge.rustunnel.com"

# Ports for incoming tunnel traffic (requires CAP_NET_BIND_SERVICE or root).
http_port    = 80
https_port   = 443

# Control-plane WebSocket port — clients connect here.
control_port = 4040

# Dashboard UI and REST API port.
dashboard_port = 8443

# ── TLS ─────────────────────────────────────────────────────────────────────
[tls]
# Paths written by Certbot (see step 6).
cert_path = "/etc/letsencrypt/live/edge.rustunnel.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/edge.rustunnel.com/privkey.pem"

# Set acme_enabled = true only if you want rustunnel to manage certs itself
# via the ACME protocol (requires Cloudflare credentials below).
# When using Certbot (recommended), leave this false.
acme_enabled = false

# ── Auth ─────────────────────────────────────────────────────────────────────
[auth]
# Strong random secret — used both as the admin token and for client auth.
# Generate: openssl rand -hex 32
admin_token  = "your-admin-token-here"
require_auth = true

# ── Database ─────────────────────────────────────────────────────────────────
[database]
# SQLite file. The directory must be writable by the rustunnel user.
path = "/var/lib/rustunnel/rustunnel.db"

# ── Logging ──────────────────────────────────────────────────────────────────
[logging]
level  = "info"
format = "json"

# Optional: write an append-only audit log (JSON-lines) for auth attempts,
# tunnel registrations, token creation/deletion, and admin actions.
# Omit or comment out to disable.
audit_log_path = "/var/lib/rustunnel/audit.log"

# ── Limits ───────────────────────────────────────────────────────────────────
[limits]
# Maximum tunnels a single authenticated session may register.
max_tunnels_per_session = 10

# Maximum simultaneous proxied connections per tunnel (semaphore).
max_connections_per_tunnel = 100

# Per-tunnel request rate limit (requests/second).
rate_limit_rps = 100

# Per-source-IP rate limit (requests/second). Set to 0 to disable.
ip_rate_limit_rps = 100

# Maximum size of a proxied HTTP request body (bytes). Default: 10 MB.
request_body_max_bytes = 10485760

# Inclusive port range reserved for TCP tunnels.
# Each active TCP tunnel consumes one port from this range.
tcp_port_range = [20000, 20099]
```

Secure the file:

```bash
chown root:rustunnel /etc/rustunnel/server.toml
chmod 640 /etc/rustunnel/server.toml
```

### 6 — TLS certificates (Let's Encrypt + Cloudflare)

Create the Cloudflare credentials file:

```bash
cat > /etc/letsencrypt/cloudflare.ini <<'EOF'
# Cloudflare API token with DNS:Edit permission for the zone.
dns_cloudflare_api_token = YOUR_CLOUDFLARE_API_TOKEN
EOF

chmod 600 /etc/letsencrypt/cloudflare.ini
```

Request a certificate covering the bare domain and the wildcard (required for HTTP subdomain tunnels):

```bash
certbot certonly \
  --dns-cloudflare \
  --dns-cloudflare-credentials /etc/letsencrypt/cloudflare.ini \
  -d "edge.rustunnel.com" \
  -d "*.edge.rustunnel.com" \
  --agree-tos \
  --email joaoh82@gmail.com
```

Certbot writes the certificate to:
```
/etc/letsencrypt/live/edge.rustunnel.com/fullchain.pem
/etc/letsencrypt/live/edge.rustunnel.com/privkey.pem
```

These paths are already set in the config above. Certbot sets up automatic renewal via a systemd timer — no further action needed.

Allow the `rustunnel` service user to read the certificates:

```bash
# Grant read access to the live/ and archive/ directories
chmod 755 /etc/letsencrypt/{live,archive}
chmod 640 /etc/letsencrypt/live/edge.rustunnel.com/*.pem
chgrp rustunnel /etc/letsencrypt/live/edge.rustunnel.com/*.pem
chgrp rustunnel /etc/letsencrypt/archive/edge.rustunnel.com/*.pem
chmod 640 /etc/letsencrypt/archive/edge.rustunnel.com/*.pem
```

### 7 — Set up systemd service

```bash
# Copy the unit file from the repository
install -Dm644 deploy/rustunnel.service /etc/systemd/system/rustunnel.service

systemctl daemon-reload
systemctl enable --now rustunnel.service

# Check it started
systemctl status rustunnel.service
journalctl -u rustunnel.service -f
```

### 8 — Open firewall ports

```bash
ufw allow 80/tcp   comment "rustunnel HTTP edge"
ufw allow 443/tcp  comment "rustunnel HTTPS edge"
ufw allow 4040/tcp comment "rustunnel control plane"
ufw allow 8443/tcp comment "rustunnel dashboard"
ufw allow 9090/tcp comment "rustunnel Prometheus metrics"

# TCP tunnel port range (must match tcp_port_range in server.toml)
ufw allow 20000:20099/tcp comment "rustunnel TCP tunnels"
```

### 9 — Verify the server is running

```bash
# Health check — use dashboard_port from server.toml (default 8443 in production)
curl http://localhost:8443/api/status

# Confirm which ports the process is actually bound to
ss -tlnp | grep rustunnel-serve

# Startup banner is visible in the logs
journalctl -u rustunnel.service --no-pager | tail -30

# Prometheus metrics
curl -s http://localhost:9090/metrics
```

> **Port reminder**: port 4040 is the control-plane WebSocket (clients connect here),
> not the dashboard. Hitting it with plain HTTP returns `HTTP/0.9` which is expected.
> The dashboard is on `dashboard_port` — check your `server.toml` if unsure.

### Updating the server

Pull the latest code, rebuild, install, and restart in one command:

```bash
cd ~/rustunnel && sudo make update-server
```

This runs `git pull` → `cargo build --release` → `install` → `systemctl restart` → `systemctl status`.

---

## Docker deployment

A full Docker guide covering both local development (self-signed cert) and
production VPS (Let's Encrypt) is available in
[**docs/docker-deployment.md**](docs/docker-deployment.md).

### Quick reference

```bash
# Build the image (includes Next.js dashboard + Rust server)
make docker-build

# Local development (self-signed cert, no auth required)
docker compose -f deploy/docker-compose.local.yml up

# Production VPS (requires deploy/server.toml to be configured first)
make docker-run

# Production + Prometheus + Grafana monitoring stack
make docker-run-monitoring

# Tail server logs
make docker-logs

# Stop everything
make docker-stop
```

### Files

| File | Purpose |
|------|---------|
| `deploy/Dockerfile` | Multi-stage build: Node.js UI → Rust server → slim runtime |
| `deploy/docker-compose.yml` | Production compose file |
| `deploy/docker-compose.local.yml` | Local development compose file |
| `deploy/server.toml` | Production server config template |
| `deploy/server.local.toml` | Local development server config |
| `deploy/prometheus.yml` | Prometheus scrape config |

---

## Client configuration

### Installation

**Option 1 — Homebrew (macOS and Linux, recommended)**

```bash
brew tap joaoh82/rustunnel
brew install rustunnel
```

Homebrew installs pre-built binaries — no Rust toolchain required.
The formula is updated automatically on every release. This installs
both `rustunnel` (the CLI client) and `rustunnel-mcp` (the MCP server
for AI agent integration).

**Option 2 — Pre-built binary**

Download the archive for your platform from the
[latest GitHub Release](https://github.com/joaoh82/rustunnel/releases/latest),
extract it, and move the `rustunnel` binary to a directory on your `$PATH`:

```bash
# Example for macOS Apple Silicon
curl -L https://github.com/joaoh82/rustunnel/releases/latest/download/rustunnel-<version>-aarch64-apple-darwin.tar.gz \
  | tar xz
sudo install -Dm755 rustunnel /usr/local/bin/rustunnel
```

Available targets:

| Platform | Archive |
|----------|---------|
| macOS Apple Silicon | `rustunnel-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `rustunnel-<version>-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc) | `rustunnel-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux x86_64 (musl, static) | `rustunnel-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `rustunnel-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `rustunnel-<version>-x86_64-pc-windows-msvc.zip` |

**Option 3 — Build from source**

Requires Rust 1.76+.

```bash
git clone https://github.com/joaoh82/rustunnel.git
cd rustunnel
cargo build --release -p rustunnel-client
sudo install -Dm755 target/release/rustunnel /usr/local/bin/rustunnel

# Or via make
make deploy-client
```

### Setup wizard

The easiest way to create your config file is the interactive setup wizard:

```bash
rustunnel setup
```

It prompts for your server address (default: `edge.rustunnel.com:4040`) and auth token, then writes `~/.rustunnel/config.yml` with a commented `tunnels:` example section.

```
rustunnel setup — create ~/.rustunnel/config.yml

Tunnel server address [edge.rustunnel.com:4040]:
Auth token (leave blank to skip): rt_live_abc123...

Created: /Users/you/.rustunnel/config.yml
Run `rustunnel start` to connect using this config.
```

After running setup, use `rustunnel start` to connect with all tunnels defined in the config, or use `rustunnel http <port>` / `rustunnel tcp <port>` for one-off tunnels.

### Quick start (CLI flags)

```bash
# Expose a local HTTP service on port 3000
rustunnel http 3000 \
  --server edge.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN

# Expose a local service with a custom subdomain
rustunnel http 3000 \
  --server edge.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN \
  --subdomain myapp

# Expose a local TCP service (e.g. a PostgreSQL database)
rustunnel tcp 5432 \
  --server edge.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN

# Disable automatic reconnection
rustunnel http 3000 --server edge.rustunnel.com:4040 --no-reconnect
```

### Config file

Default location: `~/.rustunnel/config.yml`

```yaml
# ~/.rustunnel/config.yml

# Tunnel server address (host:control_port)
server: edge.rustunnel.com:4040

# Auth token (from server admin_token or a token created via the dashboard)
auth_token: YOUR_AUTH_TOKEN

# Named tunnels started with `rustunnel start`
tunnels:
  web:
    proto: http
    local_port: 3000
    subdomain: myapp      # optional — server assigns one if omitted

  db:
    proto: tcp
    local_port: 5432
```

Start all tunnels from the config file:

```bash
rustunnel start
# or with an explicit path
rustunnel start --config /path/to/config.yml
```

### Token management

Create additional auth tokens via the dashboard API:

```bash
rustunnel token create \
  --name "ci-deploy" \
  --server edge.rustunnel.com:8443 \
  --admin-token YOUR_ADMIN_TOKEN
```

Or via `curl`:

```bash
curl -s -X POST http://edge.rustunnel.com:8443/api/tokens \
  -H "Authorization: Bearer YOUR_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"label": "ci-deploy"}'
```

---

## Port reference

| Port | Protocol | Purpose |
|------|----------|---------|
| 80 | TCP | HTTP edge — redirects to HTTPS; also ACME HTTP-01 challenge |
| 443 | TCP | HTTPS edge — TLS-terminated tunnel ingress |
| 4040 | TCP | Control-plane WebSocket — clients connect here |
| 8443 | TCP | Dashboard UI and REST API |
| 9090 | TCP | Prometheus metrics (`/metrics`) |
| 20000–20099 | TCP | TCP tunnel range (configurable via `tcp_port_range`) |

---

## Config file reference (server)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server.domain` | string | — | Base domain for tunnel URLs |
| `server.http_port` | u16 | — | HTTP edge port |
| `server.https_port` | u16 | — | HTTPS edge port |
| `server.control_port` | u16 | — | WebSocket control-plane port |
| `server.dashboard_port` | u16 | `4040` | Dashboard port |
| `tls.cert_path` | string | — | Path to TLS certificate (PEM) |
| `tls.key_path` | string | — | Path to TLS private key (PEM) |
| `tls.acme_enabled` | bool | `false` | Enable built-in ACME renewal |
| `tls.acme_email` | string | `""` | Contact email for ACME |
| `tls.acme_staging` | bool | `false` | Use Let's Encrypt staging CA |
| `tls.acme_account_dir` | string | `/var/lib/rustunnel` | ACME state directory |
| `tls.cloudflare_api_token` | string | `""` | Cloudflare DNS API token (prefer env var `CLOUDFLARE_API_TOKEN`) |
| `tls.cloudflare_zone_id` | string | `""` | Cloudflare Zone ID (prefer env var `CLOUDFLARE_ZONE_ID`) |
| `auth.admin_token` | string | — | Master auth token |
| `auth.require_auth` | bool | — | Reject unauthenticated clients |
| `database.path` | string | — | SQLite file path (`:memory:` for tests) |
| `logging.level` | string | — | `trace` / `debug` / `info` / `warn` / `error` |
| `logging.format` | string | — | `json` or `pretty` |
| `logging.audit_log_path` | string | `null` | Path for audit log (JSON-lines); omit to disable |
| `limits.max_tunnels_per_session` | usize | — | Max tunnels per connected client |
| `limits.max_connections_per_tunnel` | usize | — | Max concurrent connections per tunnel |
| `limits.rate_limit_rps` | u32 | — | Per-tunnel request rate cap (req/s) |
| `limits.ip_rate_limit_rps` | u32 | `100` | Per-source-IP rate cap (req/s); `0` = disabled |
| `limits.request_body_max_bytes` | usize | — | Max proxied request body size (bytes) |
| `limits.tcp_port_range` | [u16, u16] | — | Inclusive `[low, high]` TCP tunnel port range |

---

## Monitoring

A Prometheus metrics endpoint is available at `:9090/metrics`:

```
rustunnel_active_sessions      # gauge: connected clients
rustunnel_active_tunnels_http  # gauge: active HTTP tunnels
rustunnel_active_tunnels_tcp   # gauge: active TCP tunnels
```

Start with the full monitoring stack (Prometheus + Grafana):

```bash
make docker-run-monitoring
# Grafana:    http://localhost:3000  (admin / changeme)
# Prometheus: http://localhost:9090
```

---

## REST API

The dashboard port exposes a REST API for programmatic access to tunnels, tokens, captured requests, and tunnel history. All endpoints (except the health check) require an `Authorization: Bearer <token>` header.

**Quick reference**

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/status` | Health check (no auth) |
| `GET` | `/api/tunnels` | List active tunnels |
| `GET` | `/api/tunnels/:id` | Get a single tunnel |
| `DELETE` | `/api/tunnels/:id` | Force-close a tunnel |
| `GET` | `/api/tunnels/:id/requests` | Captured HTTP requests |
| `POST` | `/api/tunnels/:id/replay/:req_id` | Fetch stored request for replay |
| `GET` | `/api/tokens` | List API tokens |
| `POST` | `/api/tokens` | Create an API token |
| `DELETE` | `/api/tokens/:id` | Delete an API token |
| `GET` | `/api/history` | Paginated tunnel history |

Full request/response schemas, query parameters, and examples are in
[**docs/api-reference.md**](docs/api-reference.md).

A machine-readable OpenAPI 3.0 spec is served at `GET /api/openapi.json` (no auth required).

---

## AI agent integration (MCP server)

rustunnel ships a `rustunnel-mcp` binary that implements the
[Model Context Protocol](https://spec.modelcontextprotocol.io) over stdio,
letting AI agents (Claude, GPT-4o, custom agents) open and manage tunnels
without any manual intervention.

### Quick setup (Claude Desktop — hosted server)

```json
{
  "mcpServers": {
    "rustunnel": {
      "command": "rustunnel-mcp",
      "args": [
        "--server", "edge.rustunnel.com:4040",
        "--api",    "https://edge.rustunnel.com:8443"
      ]
    }
  }
}
```

### Available tools

| Tool | Description |
|------|-------------|
| `create_tunnel` | Spawn a tunnel and return the public URL |
| `list_tunnels` | List all active tunnels |
| `close_tunnel` | Force-close a tunnel by ID |
| `get_connection_info` | Return the CLI command for cloud/sandbox agents |
| `get_tunnel_history` | Retrieve past tunnel activity |

### Installation

**Homebrew (macOS and Linux)** — installs `rustunnel-mcp` alongside the CLI:

```bash
brew tap joaoh82/rustunnel
brew install rustunnel
```

**Build from source:**

```bash
make release-mcp
sudo install -m755 target/release/rustunnel-mcp /usr/local/bin/rustunnel-mcp
```

Full setup guide, configuration options, and workflow examples are in
[**docs/mcp-server.md**](docs/mcp-server.md).

### OpenClaw skill

rustunnel ships an [OpenClaw](https://openclaw.dev) skill that gives any
OpenClaw-compatible AI agent first-class knowledge of rustunnel — config file
format, authentication, tool signatures, and common workflows — without you
having to explain it.

**Skill file:** [`skills/rustunnel/SKILL.md`](skills/rustunnel/SKILL.md)

**What it covers:**

| Topic | Details |
|-------|---------|
| Config file | Location (`~/.rustunnel/config.yml`), format, named tunnels |
| First-time setup | `rustunnel setup` wizard or manual config creation |
| MCP tools | `create_tunnel`, `list_tunnels`, `close_tunnel`, `get_connection_info`, `get_tunnel_history` |
| Workflows | Webhook testing, demo sharing, cloud sandbox (no subprocess), named tunnels |
| Security | Token handling, file permissions, HTTPS-only transport |

**To load the skill in Claude Code:**

```bash
/skills load skills/rustunnel/SKILL.md
```

Once loaded, you can ask the agent things like:

> "Expose my local port 3000 as an HTTPS tunnel using rustunnel."

> "List my active tunnels and close the one forwarding port 5432."

> "Set up my rustunnel config file with my token."

The skill instructs the agent to read credentials from `~/.rustunnel/config.yml`
automatically, so you won't be prompted for your token on every invocation.

---

## Roadmap

A detailed list of shipped features and planned future work is maintained in
[**docs/ROADMAP.md**](docs/ROADMAP.md).

---

## Contributing

Contributions are welcome! Please follow these steps:

1. **Fork** the repository and create a feature branch from `main`.
2. Run `make install-hooks` once after cloning to activate the pre-push quality gate.
3. Make your changes. Ensure `make check` (fmt + Clippy) and `make test` pass locally.
4. Open a **Pull Request** with a clear description of what changed and why.
5. A maintainer will review and merge once CI is green.

### Guidelines

- Keep PRs focused — one logical change per PR.
- Add or update tests for any new behaviour.
- Follow the existing code style; `cargo fmt` is enforced by CI.
- For larger changes or new features, open an issue first to discuss the approach.

---

## License

This project is licensed under the **MIT License** — see the [LICENSE](LICENSE) file for details.

---

## Contact

**João Henrique Machado Silva**

- GitHub: [@joaoh82](https://github.com/joaoh82)
- Project: [github.com/joaoh82/rustunnel](https://github.com/joaoh82/rustunnel)
- Issues & feature requests: [GitHub Issues](https://github.com/joaoh82/rustunnel/issues)
