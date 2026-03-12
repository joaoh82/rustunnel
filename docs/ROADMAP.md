# rustunnel Roadmap

This document tracks the features that have already shipped and ideas planned for future releases. It is a living reference — items may be re-prioritised or added as the project evolves.

---

## Implemented

### Core tunnel engine
- [x] HTTP tunnel proxying with automatic subdomain routing (`<id>.yourdomain.com`)
- [x] Custom subdomain support (`--subdomain myapp`)
- [x] TCP tunnel proxying with dynamic port allocation from a configurable range
- [x] yamux stream multiplexing over a single WebSocket connection
- [x] Automatic client reconnection with configurable retry logic
- [x] Graceful shutdown — drains active sessions with a 30-second timeout on SIGINT/SIGTERM

### TLS & security
- [x] TLS termination on the HTTPS edge using rustls
- [x] Static PEM certificate support (BYO cert from Let's Encrypt, Certbot, etc.)
- [x] Built-in ACME client for automatic certificate provisioning and renewal (Cloudflare DNS-01 challenge)
- [x] Per-tunnel request rate limiting (requests/second)
- [x] Per-source-IP rate limiting
- [x] Request body size cap
- [x] Maximum tunnels per session limit
- [x] Maximum concurrent connections per tunnel limit (semaphore)

### Authentication & tokens
- [x] Admin token authentication (static secret in server config)
- [x] Database-backed API tokens (create, list, delete)
- [x] Token scope field for future RBAC use
- [x] Token last-used timestamp tracking
- [x] Per-token tunnel count tracking
- [x] Tunnel history page in the dashboard (paginated table with protocol filter, duration, token attribution)
- [x] Token management via CLI (`rustunnel token create / list / delete`)
- [x] Token management via Dashboard UI

### Dashboard UI
- [x] Live dashboard built with Next.js (static export embedded in server binary)
- [x] Active sessions panel with real-time polling
- [x] Active tunnels panel (HTTP and TCP)
- [x] Live request inspector (captures HTTP requests proxied through tunnels)
- [x] API token management panel (create / view / delete tokens with one-time raw token display)
- [x] Per-token tunnel usage counter

### Observability
- [x] Structured JSON logging (via `tracing` + `tracing-subscriber`)
- [x] Append-only audit log (JSON-lines) for auth, tunnel, and token events
- [x] Prometheus metrics endpoint (`/metrics` on `:9090`)
  - `rustunnel_active_sessions`
  - `rustunnel_active_tunnels_http`
  - `rustunnel_active_tunnels_tcp`
- [x] SQLite-backed tunnel activity log (`tunnel_log` table with token attribution)

### Deployment
- [x] Multi-stage Dockerfile for minimal production images
- [x] Docker Compose stack (server + optional Prometheus + Grafana)
- [x] systemd service unit with dedicated system user
- [x] `make deploy` / `make update-server` helpers for bare-metal deployments
- [x] Pre-built Grafana dashboard for tunnel metrics

### Developer experience
- [x] Cargo workspace with separate `rustunnel-server`, `rustunnel-client`, and `rustunnel-protocol` crates
- [x] Integration test suite (spins up a real server on random ports, tests auth, HTTP/TCP tunnels, reconnection)
- [x] GitHub Actions CI (format check + Clippy + full test suite)
- [x] Pre-push git hook mirroring CI checks (`make install-hooks`)
- [x] Local development config (`deploy/local/server.toml`) and self-signed cert setup instructions

---

## Planned / Ideas

Items below are not committed to any release timeline. They represent directions the project may grow in.

### Short-term
- [ ] Pre-built release binaries for Linux (x86_64, aarch64) and macOS via GitHub Releases
- [ ] Shell completions for the CLI (bash, zsh, fish)
- [ ] `rustunnel status` command to inspect the active connection and registered tunnels
- [ ] Extended Prometheus metrics (bytes proxied, request latency histograms, error rates)

### Medium-term
- [ ] Token RBAC — enforce scope restrictions (e.g. `http-only`, `tcp-only`, read-only dashboard)
- [ ] Bandwidth limiting per tunnel
- [ ] Webhook notifications on tunnel connect / disconnect events
- [ ] Dashboard dark mode
- [ ] Windows support for the client binary
- [ ] Config file hot-reload (SIGHUP) without restarting the server
- [ ] Health check / heartbeat endpoint for load balancer probing

### Long-term / Exploratory
- [ ] SSH tunnel support (`rustunnel ssh`)
- [ ] Custom domain per tunnel (BYOD — bring your own domain with DNS verification)
- [ ] Multi-user / team management with role-based access control
- [ ] Traffic inspector with request replay in the dashboard
- [ ] Tunnel persistence across server restarts (reconnect to the same subdomain/port)
- [ ] Geographic routing — multiple server regions behind a single hostname
- [ ] mTLS client authentication
- [ ] Plugin / middleware system for request transformation and filtering
- [ ] Distributed server mode (multiple instances sharing state via a database)

---

## Changelog highlights

| Version | Highlights |
|---------|-----------|
| 0.1.0 | Initial release — HTTP/TCP tunnels, TLS, admin token auth, dashboard, Prometheus metrics |
| 0.2.0 | API token management (create/list/delete), tunnel activity log, per-token tunnel counts |
