# rustunnel

A self-hosted, ngrok-style secure tunnel server written in Rust. Expose local services through a public server over encrypted WebSocket connections with TLS termination, HTTP/TCP proxying, a live dashboard, Prometheus metrics, and audit logging.

---

## Table of Contents

- [Architecture overview](#architecture-overview)
- [Requirements](#requirements)
- [Local development setup](#local-development-setup)
  - [Build](#build)
  - [Run tests](#run-tests)
  - [Run the server locally](#run-the-server-locally)
  - [Run the client locally](#run-the-client-locally)
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
- [Docker deployment](#docker-deployment)
- [Client configuration](#client-configuration)
  - [Quick start (CLI flags)](#quick-start-cli-flags)
  - [Config file](#config-file)
  - [Token management](#token-management)
- [Port reference](#port-reference)
- [Config file reference (server)](#config-file-reference-server)
- [Monitoring](#monitoring)

---

## Architecture overview

```
                        ┌──────────────────────────────────────────┐
                        │           rustunnel-server               │
                        │                                          │
Internet ──── :80 ─────▶│  HTTP edge (301 → HTTPS)                 │
Internet ──── :443 ────▶│  HTTPS edge  ──▶ yamux stream ──▶ client │
Client ───── :4040 ────▶│  Control-plane WebSocket (TLS)           │
Browser ──── :8080 ────▶│  Dashboard UI + REST API                 │
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

The integration test suite spins up a real server on random ports and exercises auth, HTTP tunnels, TCP tunnels, and reconnection logic.

```bash
# Full suite (unit + integration)
cargo test --workspace

# Or via Makefile
make test

# With output visible
cargo test --workspace -- --nocapture
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

---

## Production deployment (Ubuntu / systemd)

The steps below match a deployment where:
- Domain: `tunnel.rustunnel.com`
- Wildcard DNS: `*.tunnel.rustunnel.com → <server IP>`
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
domain       = "tunnel.rustunnel.com"

# Ports for incoming tunnel traffic (requires CAP_NET_BIND_SERVICE or root).
http_port    = 80
https_port   = 443

# Control-plane WebSocket port — clients connect here.
control_port = 4040

# Dashboard UI and REST API port.
dashboard_port = 8080

# ── TLS ─────────────────────────────────────────────────────────────────────
[tls]
# Paths written by Certbot (see step 6).
cert_path = "/etc/letsencrypt/live/tunnel.rustunnel.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/tunnel.rustunnel.com/privkey.pem"

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
  -d "tunnel.rustunnel.com" \
  -d "*.tunnel.rustunnel.com" \
  --agree-tos \
  --email your@email.com
```

Certbot writes the certificate to:
```
/etc/letsencrypt/live/tunnel.rustunnel.com/fullchain.pem
/etc/letsencrypt/live/tunnel.rustunnel.com/privkey.pem
```

These paths are already set in the config above. Certbot sets up automatic renewal via a systemd timer — no further action needed.

Allow the `rustunnel` service user to read the certificates:

```bash
# Grant read access to the live/ and archive/ directories
chmod 755 /etc/letsencrypt/{live,archive}
chmod 640 /etc/letsencrypt/live/tunnel.rustunnel.com/*.pem
chgrp rustunnel /etc/letsencrypt/live/tunnel.rustunnel.com/*.pem
chgrp rustunnel /etc/letsencrypt/archive/tunnel.rustunnel.com/*.pem
chmod 640 /etc/letsencrypt/archive/tunnel.rustunnel.com/*.pem
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
ufw allow 8080/tcp comment "rustunnel dashboard"
ufw allow 9090/tcp comment "rustunnel Prometheus metrics"

# TCP tunnel port range (must match tcp_port_range in server.toml)
ufw allow 20000:20099/tcp comment "rustunnel TCP tunnels"
```

### 9 — Verify the server is running

```bash
# Health check — use dashboard_port from server.toml (default 8080 in production)
curl http://localhost:8080/api/status

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

---

## Docker deployment

A multi-stage Dockerfile and Docker Compose file are included in `deploy/`.

**Prerequisites:** create `deploy/server.toml` (copy from step 5 above and adjust paths if needed — inside the container the cert paths from Let's Encrypt won't be available unless you bind-mount `/etc/letsencrypt`).

```bash
# Build image
make docker-build

# Start server only
make docker-run

# Start server + Prometheus + Grafana
make docker-run-monitoring

# Tail logs
make docker-logs

# Stop everything
make docker-stop
```

Mount your host Let's Encrypt directory into the container by adding to `deploy/docker-compose.yml`:

```yaml
volumes:
  - ./server.toml:/etc/rustunnel/server.toml:ro
  - /etc/letsencrypt:/etc/letsencrypt:ro   # add this line
  - rustunnel-data:/var/lib/rustunnel
```

---

## Client configuration

Install the client binary:

```bash
# From source
cargo build --release -p rustunnel-client
install -Dm755 target/release/rustunnel /usr/local/bin/rustunnel

# Or via make
make deploy-client
```

### Quick start (CLI flags)

```bash
# Expose a local HTTP service on port 3000
rustunnel http 3000 \
  --server tunnel.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN

# Expose a local service with a custom subdomain
rustunnel http 3000 \
  --server tunnel.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN \
  --subdomain myapp

# Expose a local TCP service (e.g. a PostgreSQL database)
rustunnel tcp 5432 \
  --server tunnel.rustunnel.com:4040 \
  --token YOUR_AUTH_TOKEN

# Disable automatic reconnection
rustunnel http 3000 --server tunnel.rustunnel.com:4040 --no-reconnect
```

### Config file

Default location: `~/.rustunnel/config.yml`

```yaml
# ~/.rustunnel/config.yml

# Tunnel server address (host:control_port)
server: tunnel.rustunnel.com:4040

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
  --server tunnel.rustunnel.com:8080 \
  --admin-token YOUR_ADMIN_TOKEN
```

Or via `curl`:

```bash
curl -s -X POST http://tunnel.rustunnel.com:8080/api/tokens \
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
| 8080 | TCP | Dashboard UI and REST API |
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
