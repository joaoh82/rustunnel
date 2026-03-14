# Docker Deployment Guide

This guide covers two scenarios:

- **[Scenario A — Local development](#scenario-a--local-development)**: run the server on your laptop using a self-signed certificate.
- **[Scenario B — VPS production](#scenario-b--vps-production)**: deploy on a cloud server with a real domain and Let's Encrypt TLS.

---

## Prerequisites

| Tool | Version | Notes |
|------|---------|-------|
| Docker Engine | 24+ | [Install docs](https://docs.docker.com/engine/install/) |
| Docker Compose | v2 (plugin) | Bundled with Docker Desktop; `docker compose` (no hyphen) |
| Git | any | To clone the repository |

Clone the repository once:

```bash
git clone https://github.com/joaoh82/rustunnel.git
cd rustunnel
```

---

## How the Docker image works

The `deploy/Dockerfile` is a three-stage build:

1. **`ui-builder`** — installs Node 20, runs `next build`, and produces the static dashboard export under `out/`.
2. **`builder`** — compiles the Rust server. The Next.js `out/` directory is copied into `crates/rustunnel-server/src/dashboard/assets/` so `rust-embed` can bake it into the binary at compile time.
3. **`runtime`** — minimal `debian:bookworm-slim` image containing only the server binary and `ca-certificates`.

Because both the UI and the Rust binary are built inside Docker you do **not** need Node.js or Rust installed on the host to build or run the image.

---

## Scenario A — Local development

Use this when you want to run a full server stack on your laptop for testing or development.

### 1 — Generate a self-signed certificate

```bash
mkdir -p /tmp/rustunnel-certs

openssl req -x509 -newkey rsa:2048 \
  -keyout /tmp/rustunnel-certs/key.pem \
  -out    /tmp/rustunnel-certs/cert.pem \
  -days 365 -nodes \
  -subj "/CN=localhost"
```

The compose file mounts this directory into the container at `/certs`.
To use a different path, set the `CERT_DIR` environment variable before running compose.

### 2 — Build the image

```bash
make docker-build
# equivalent: docker build -f deploy/Dockerfile -t rustunnel-server:latest .
```

The first build takes several minutes (Rust + Node.js compilation).
Subsequent builds use Docker layer caching and are much faster unless `Cargo.lock` or `package-lock.json` change.

### 3 — Start the server

```bash
docker compose -f deploy/docker-compose.local.yml up
```

Add `-d` to detach:

```bash
docker compose -f deploy/docker-compose.local.yml up -d
```

### 4 — Verify it is running

```bash
# Health check (HTTP — no TLS required for dashboard in local mode)
curl http://localhost:4041/health

# Open the dashboard in a browser
open http://localhost:4041
```

### 5 — Connect a client

```bash
# Expose a local service running on port 3000
rustunnel http 3000 \
  --server localhost:4040 \
  --token dev-secret-change-me \
  --insecure
```

> `--insecure` skips TLS verification. Required for self-signed certificates.
> **Never** use this flag against a production server.

### 6 — Stop the server

```bash
docker compose -f deploy/docker-compose.local.yml down
```

### Port reference (local)

| Port | Purpose |
|------|---------|
| `4040` | Control-plane WebSocket — clients connect here |
| `4041` | Dashboard UI and REST API |
| `8080` | HTTP edge (tunnel ingress, redirects to HTTPS) |
| `8443` | HTTPS edge (TLS-terminated tunnel ingress) |
| `20000–20099` | TCP tunnel range |

### Reaching HTTP tunnel URLs locally

HTTP tunnels use subdomains (e.g. `http://abc123.localhost:8080`).
Browsers do not resolve `*.localhost` by default. Two options:

**Option A — curl with a Host header (no setup)**

```bash
curl -v -H "Host: abc123.localhost" http://localhost:8080/
```

**Option B — wildcard DNS via dnsmasq (macOS)**

```bash
brew install dnsmasq
echo "address=/.localhost/127.0.0.1" | sudo tee -a $(brew --prefix)/etc/dnsmasq.conf
sudo brew services start dnsmasq
sudo mkdir -p /etc/resolver
echo "nameserver 127.0.0.1" | sudo tee /etc/resolver/localhost
```

Then visit `http://abc123.localhost:8080` in the browser.

---

## Scenario B — VPS production

Use this when you have a cloud server (Ubuntu 22.04 or later recommended) with a public IP address.

### Assumptions

| Item | Example value |
|------|--------------|
| Domain | `edge.rustunnel.com` |
| Wildcard DNS | `*.edge.rustunnel.com → <server public IP>` |
| TLS | Let's Encrypt via Certbot + Cloudflare DNS challenge |
| OS | Ubuntu 22.04 LTS |

Set up the wildcard DNS record with your DNS provider before continuing.
Both `edge.rustunnel.com` (bare) and `*.edge.rustunnel.com` (wildcard) must resolve to your server IP — the wildcard is required so HTTP tunnel subdomains work.

### 1 — Install dependencies on the VPS

```bash
apt update && apt install -y \
  git curl \
  certbot python3-certbot-dns-cloudflare \
  docker.io docker-compose-plugin

# Enable Docker to start on boot
systemctl enable --now docker
```

### 2 — Clone the repository

```bash
git clone https://github.com/joaoh82/rustunnel.git
cd rustunnel
```

### 3 — Obtain TLS certificates

Create the Cloudflare credentials file:

```bash
mkdir -p /etc/letsencrypt
cat > /etc/letsencrypt/cloudflare.ini <<'EOF'
# Cloudflare API token with DNS:Edit permission for the zone.
dns_cloudflare_api_token = YOUR_CLOUDFLARE_API_TOKEN
EOF
chmod 600 /etc/letsencrypt/cloudflare.ini
```

Request the certificate (bare domain + wildcard):

```bash
certbot certonly \
  --dns-cloudflare \
  --dns-cloudflare-credentials /etc/letsencrypt/cloudflare.ini \
  -d "edge.rustunnel.com" \
  -d "*.edge.rustunnel.com" \
  --agree-tos \
  --email your@email.com
```

Certbot writes the PEM files to:
```
/etc/letsencrypt/live/edge.rustunnel.com/fullchain.pem
/etc/letsencrypt/live/edge.rustunnel.com/privkey.pem
```

Certbot installs a systemd timer for automatic renewal — no further action needed.

### 4 — Configure the server

Generate a strong admin token:

```bash
openssl rand -hex 32
```

Edit `deploy/server.toml` — set **at minimum**:

```toml
[server]
domain = "edge.rustunnel.com"   # ← your domain

[tls]
cert_path = "/etc/letsencrypt/live/edge.rustunnel.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/edge.rustunnel.com/privkey.pem"

[auth]
admin_token  = "PASTE_YOUR_GENERATED_TOKEN_HERE"
require_auth = true
```

The file is mounted read-only into the container.

### 5 — Grant the container access to the certificates

The container runs as a non-root user (`rustunnel`). Certbot sets restrictive permissions on the `live/` and `archive/` directories by default:

```bash
# Allow read access to cert directories
chmod 755 /etc/letsencrypt/{live,archive}
chmod 640 /etc/letsencrypt/live/edge.rustunnel.com/*.pem
chmod 640 /etc/letsencrypt/archive/edge.rustunnel.com/*.pem
```

> **Alternative**: if you prefer not to relax Certbot permissions, copy the certs
> to a dedicated directory and set up a Certbot post-hook to refresh the copies
> after each renewal.

### 6 — Build the Docker image

```bash
docker build -f deploy/Dockerfile -t rustunnel-server:latest .
```

### 7 — Start the server

```bash
docker compose -f deploy/docker-compose.yml up -d
```

The `docker-compose.yml` mounts:
- `./server.toml` → `/etc/rustunnel/server.toml` (read-only)
- `/etc/letsencrypt` is **not** mounted by default — add the line below to the server service's `volumes` section before starting:

```yaml
volumes:
  - ./server.toml:/etc/rustunnel/server.toml:ro
  - /etc/letsencrypt:/etc/letsencrypt:ro   # ← add this
  - rustunnel-data:/var/lib/rustunnel
```

### 8 — Open firewall ports

```bash
ufw allow 80/tcp    comment "rustunnel HTTP edge"
ufw allow 443/tcp   comment "rustunnel HTTPS edge"
ufw allow 4040/tcp  comment "rustunnel control plane"
ufw allow 8443/tcp  comment "rustunnel dashboard"
ufw allow 9090/tcp  comment "rustunnel Prometheus metrics"
ufw allow 20000:20099/tcp comment "rustunnel TCP tunnels"
```

> Port 9090 only needs to be open if you have an external Prometheus scraper.
> It is safe to leave it closed if you are running Prometheus on the same host
> (it reaches the metrics endpoint over the Docker bridge network).

### 9 — Verify the deployment

```bash
# Health check
curl https://edge.rustunnel.com:8443/health

# Prometheus metrics
curl -s http://localhost:9090/metrics

# Tail logs
docker compose -f deploy/docker-compose.yml logs -f rustunnel-server
```

### 10 — Connect a client

```bash
rustunnel http 3000 \
  --server edge.rustunnel.com:4040 \
  --token YOUR_ADMIN_TOKEN
```

### Port reference (production)

| Port | Purpose |
|------|---------|
| `80` | HTTP edge (redirects to HTTPS; handles ACME HTTP-01 if enabled) |
| `443` | HTTPS edge (TLS-terminated tunnel ingress) |
| `4040` | Control-plane WebSocket — clients connect here |
| `8443` | Dashboard UI and REST API |
| `9090` | Prometheus metrics (`/metrics`) |
| `20000–20099` | TCP tunnel range (configurable via `tcp_port_range`) |

---

## Optional: monitoring stack (Prometheus + Grafana)

Both compose files expose the metrics endpoint to the `rustunnel` Docker network.
The Prometheus service in `docker-compose.yml` scrapes it automatically.

```bash
# Start server + Prometheus + Grafana
docker compose -f deploy/docker-compose.yml --profile monitoring up -d

# URLs
# Grafana:    http://<host>:3000   (admin / changeme — change GF_SECURITY_ADMIN_PASSWORD)
# Prometheus: http://<host>:9090
```

To change the Grafana admin password before starting, set the environment variable:

```bash
export GRAFANA_PASSWORD="$(openssl rand -hex 16)"
docker compose -f deploy/docker-compose.yml --profile monitoring up -d
```

---

## Useful make targets

| Target | Description |
|--------|-------------|
| `make docker-build` | Build the Docker image |
| `make docker-run` | Start the server container only |
| `make docker-run-monitoring` | Start server + Prometheus + Grafana |
| `make docker-logs` | Tail server container logs |
| `make docker-stop` | Stop and remove all containers |

---

## Updating

Pull the latest code and rebuild:

```bash
git pull
docker build -f deploy/Dockerfile -t rustunnel-server:latest .
docker compose -f deploy/docker-compose.yml up -d --force-recreate rustunnel-server
```

The `--force-recreate` flag restarts the container with the new image while
leaving the `rustunnel-data` volume (SQLite database + state) intact.

---

## Troubleshooting

### Container exits immediately

```bash
docker compose -f deploy/docker-compose.yml logs rustunnel-server
```

Common causes:
- **Config not mounted** — ensure `deploy/server.toml` exists and the volume path is correct.
- **Cert files not readable** — check permissions on `/etc/letsencrypt/` (see step 5).
- **Port already in use** — check `ss -tlnp | grep -E '80|443|4040|8443'`.

### Dashboard shows "dashboard assets not found"

The dashboard assets were not embedded at compile time. This happens if you built the Rust binary before running `npm run build` (or outside Docker). Rebuild the image with `docker build` — the multi-stage Dockerfile handles the UI build automatically.

### `--insecure` flag required even in production

This means the client is connecting to a server with a self-signed cert. Verify that the cert paths in `server.toml` point to the Let's Encrypt PEM files and that those files are accessible inside the container.

### Prometheus shows no data

Check that `deploy/prometheus.yml` targets `rustunnel-server:9090` and that both services are on the same Docker network (`rustunnel`). The metrics endpoint is not exposed on the host by default — Prometheus reaches it over the bridge network.
