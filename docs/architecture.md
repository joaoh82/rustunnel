# rustunnel — System Architecture

This document describes the internal architecture of rustunnel: its subsystems, data flows, concurrency model, and the protocol used between client and server.

---

## Table of Contents

1. [Overview](#overview)
2. [High-Level Topology](#high-level-topology)
3. [Server Subsystems](#server-subsystems)
4. [Client Architecture](#client-architecture)
5. [Control Protocol](#control-protocol)
6. [Data Plane — yamux over WebSocket](#data-plane--yamux-over-websocket)
7. [Per-Connection Flow](#per-connection-flow)
8. [Concurrency Model](#concurrency-model)
9. [TLS and Security](#tls-and-security)
10. [Metrics and Observability](#metrics-and-observability)
11. [Component Dependency Graph](#component-dependency-graph)
12. [Crate Structure](#crate-structure)

---

## Overview

rustunnel is a **self-hosted reverse tunnel**: it lets a client behind NAT or a firewall expose local TCP services to the internet via a central server that has a public IP address.

It is architecturally similar to ngrok or Cloudflare Tunnel, but designed to be simple, auditable, and self-hosted.

Key design choices:
- **WebSocket transport** — works through any HTTP proxy or firewall that allows HTTPS.
- **yamux multiplexing** — a single data WebSocket carries streams for all proxied connections simultaneously.
- **Two-connection model** — control plane (JSON frames) and data plane (binary yamux frames) are separated.
- **Tokio async runtime** — all I/O is non-blocking; each session runs in a handful of lightweight tasks.
- **TLS everywhere** — all external connections are encrypted via rustls (ACME or static PEM).

---

## High-Level Topology

```
                         INTERNET
                            │
              ┌─────────────▼──────────────┐
              │        rustunnel-server      │
              │                              │
              │  ┌──────────┐  ┌──────────┐ │
 Browser  ───►│  │HTTP Edge │  │TCP Edge  │ │
 / Client     │  │:80/:443  │  │:xxxxx    │ │
              │  └────┬─────┘  └────┬─────┘ │
              │       │             │       │
              │  ┌────▼─────────────▼─────┐ │
              │  │      TunnelCore         │ │
              │  │  (routing table +       │ │
              │  │   pending conn map)     │ │
              │  └────────────┬───────────┘ │
              │               │             │
              │  ┌────────────▼───────────┐ │
              │  │  Control-Plane WS      │ │
              │  │  :9000  /_control      │ │
              │  │  :9000  /_data/<id>    │ │
              │  └────────────────────────┘ │
              └──────────────┬──────────────┘
                             │  WebSocket (TLS)
                    ┌────────▼──────────┐
                    │  rustunnel client  │
                    │                    │
                    │  ┌──────────────┐ │
                    │  │ Control loop  │ │
                    │  └──────────────┘ │
                    │  ┌──────────────┐ │
                    │  │ yamux driver  │ │
                    │  └──────────────┘ │
                    │  ┌──────────────┐ │
                    │  │ Proxy tasks   │ │
                    │  └──────────────┘ │
                    └────────┬──────────┘
                             │  TCP
              ┌──────────────▼───────────────┐
              │         Local Service          │
              │  (web server, SSH, DB, etc.)   │
              └───────────────────────────────┘
```

---

## Server Subsystems

The server is a single binary (`rustunnel-server`) composed of six concurrently running subsystems:

```
rustunnel-server
├── a) Control-plane WebSocket server  (:9000)
│      /_control  — JSON control frames
│      /_data/<session_id>  — binary yamux frames
├── b) HTTP + HTTPS edge proxy  (:80, :443)
├── c) TCP edge proxy  (dynamic ports)
├── d) Dashboard REST API + SPA  (:4040)
├── e) Prometheus metrics endpoint  (:9090)
└── f) ACME certificate renewal task (background)
```

All subsystems share a single `Arc<TunnelCore>` routing table.

### a) Control-Plane WebSocket Server

Handles two routes:

- **`/_control`** — One persistent WebSocket per client. Manages authentication, tunnel registration, and heartbeats.
- **`/_data/<session_id>`** — One persistent WebSocket per client session. Carries raw yamux frames for all proxied connections belonging to that session.

Each `/_control` connection spawns a **session task** that runs the control loop for that client.

### b) HTTP / HTTPS Edge Proxy

Listens on ports 80 and 443. For each incoming HTTP request:

1. Extracts the `Host` header subdomain (e.g. `myapp` from `myapp.tunnel.example.com`).
2. Looks up the subdomain in `TunnelCore.http_routes`.
3. If found, allocates a `conn_id`, stores a oneshot sender in `TunnelCore.pending_conns`, and sends a `NewConnection` frame to the owning session.
4. Waits for the yamux stream to arrive (delivered by the session's yamux driver).
5. Copies bytes bidirectionally between the incoming HTTP connection and the yamux stream.

Rate limiting (per-IP sliding window and per-tunnel token bucket) is enforced before step 2.

### c) TCP Edge Proxy

For each registered TCP tunnel, the server allocates a port from a configured range and spawns a listener. The per-connection flow is identical to HTTP but uses the port-based `tcp_routes` lookup instead of the subdomain-based `http_routes` lookup.

### d) Dashboard

Serves a React SPA and a REST API for:
- Listing active sessions and tunnels
- Creating and revoking API tokens (stored in SQLite)
- Viewing a live request capture feed (via Server-Sent Events)
- Viewing audit logs

### e) Prometheus Metrics

Exposes three gauges at `http://<server>:9090/metrics`:

| Metric | Description |
|--------|-------------|
| `rustunnel_active_sessions` | Number of connected clients |
| `rustunnel_active_tunnels_http` | Number of active HTTP tunnels |
| `rustunnel_active_tunnels_tcp` | Number of active TCP tunnels |

### f) ACME Certificate Renewal

If `acme_enabled = true` in the server config, a background task periodically checks whether the TLS certificate needs renewal and triggers an ACME challenge. The certificate is hot-swapped into all TLS listeners without restart.

---

## Client Architecture

The client is a single binary (`rustunnel`) with three main concurrent pieces:

```
rustunnel process
│
├── main_loop task (async)
│     select! {
│       Ctrl-C         → clean shutdown
│       ping_interval  → send Ping frame
│       stream_rx.recv → match stream with pending NewConnection
│       ctrl_ws.next   → handle NewConnection / Pong / etc.
│     }
│
├── drive_client_mux task (spawned)
│     loop {
│       poll_next_inbound(yamux conn)
│       → read 16-byte conn_id prefix
│       → send (conn_id, stream) to stream_rx channel
│     }
│
└── proxy tasks (one per connection, spawned)
      tokio::io::copy_bidirectional(
        yamux_stream ↔ TcpStream to local service
      )
```

### State machines

The main loop maintains two buffering maps to handle the race between two asynchronous events that must be correlated:

| Map | Key | Value | Purpose |
|-----|-----|-------|---------|
| `pending_conns` | `conn_id` | `local_addr` | `NewConnection` arrived before the yamux stream |
| `pending_streams` | `conn_id` | `YamuxStream` | yamux stream arrived before `NewConnection` |

When both halves arrive (in either order), a proxy task is spawned and both entries are removed.

---

## Control Protocol

Control frames are JSON objects sent as **binary WebSocket messages**. They use serde's `{ "type": "...", ...fields }` envelope.

### Frame types

```
Client → Server                     Server → Client
─────────────────────────────────   ─────────────────────────────────
Auth                                AuthOk
  token: string                       session_id: uuid
  client_version: string              server_version: string
                                    AuthError
                                      message: string

RegisterTunnel                      TunnelRegistered
  request_id: string                  request_id: string
  protocol: http|tcp                  tunnel_id: uuid
  subdomain?: string                  public_url: string
  local_addr: string                  assigned_port?: u16
                                    TunnelError
                                      request_id: string
                                      message: string

Ping                                NewConnection
  timestamp: u64 (ms)                 conn_id: uuid
                                      client_addr: string
Pong                                  protocol: http|tcp
  timestamp: u64 (ms)
                                    Ping / Pong (same as client→server)
```

### Handshake sequence

```
Client                              Server
  │                                   │
  │──── WebSocket upgrade ────────────►│  wss://<server>/_control
  │                                   │
  │──── Auth ─────────────────────────►│
  │        token, client_version       │
  │                                   │  validate token against DB
  │◄─── AuthOk ───────────────────────│
  │        session_id, server_version  │
  │                                   │
  │──── RegisterTunnel ───────────────►│  (one per tunnel)
  │        request_id, protocol, …     │
  │                                   │  allocate subdomain/port
  │◄─── TunnelRegistered ─────────────│
  │        public_url, assigned_port   │
  │                                   │
  │──── WebSocket upgrade ────────────►│  wss://<server>/_data/<session_id>
  │                                   │  (data plane, runs in parallel)
  │                                   │
  │◄════════════════════════════════ normal operation ══════════════════════════►│
  │                                   │
  │  every 30 s:                      │
  │──── Ping ─────────────────────────►│
  │◄─── Pong ─────────────────────────│
  │                                   │
  │  on incoming external connection:  │
  │◄─── NewConnection ────────────────│
  │        conn_id, client_addr        │
  │                                   │
  │  (yamux stream arrives separately  │
  │   on the data WebSocket)           │
```

### Heartbeat

- Client sends `Ping` every **30 seconds**.
- Server must respond with `Pong` within **10 seconds**.
- If no `Pong` arrives within the deadline, the client disconnects with `"heartbeat timeout"` and reconnects.

---

## Data Plane — yamux over WebSocket

The data plane uses **yamux** (a stream multiplexer similar to HTTP/2 framing) over the data WebSocket. This allows a single WebSocket connection to carry streams for all proxied connections simultaneously.

### WsCompat adapter

Because yamux requires `futures::io::{AsyncRead, AsyncWrite}` but WebSocket is message-oriented, both server and client use a `WsCompat<S>` wrapper:

```
WebSocket (message-oriented)
         │
    WsCompat<S>
    ┌────────────────────────────────────────┐
    │  Read:  dequeue binary WS frames,       │
    │         present as a byte stream        │
    │  Write: wrap byte slices in binary      │
    │         WS frames and send              │
    └────────────────────────────────────────┘
         │
yamux Connection (stream-oriented)
```

### Mode assignment

yamux uses stream IDs to multiplex; "client" mode uses odd IDs, "server" mode uses even IDs:

| Side | yamux Mode | Role |
|------|-----------|------|
| Server | `Mode::Client` | Opens streams, writes first (forces SYN frame) |
| Client | `Mode::Server` | Accepts inbound streams via `poll_next_inbound` |

This assignment is intentional. yamux 0.13 uses **lazy SYN**: a new stream does not send a SYN frame until the first write. If the server waited as `Mode::Server` (accepting), and the client opened a stream as `Mode::Client` (opening) but never wrote, the connection would deadlock. By making the **server** the yamux client (opener+writer), it forces the SYN+DATA immediately, unblocking the actual client's `poll_next_inbound`.

### 16-byte conn_id prefix

When the server opens a yamux stream for a new proxied connection, it immediately writes the **16-byte raw UUID** (`conn_id.as_bytes()`) into the stream before any proxy data. This allows the client's yamux driver to correlate the stream with the `NewConnection` control frame that named the same `conn_id`.

```
Server yamux driver                 Client yamux driver
  │                                   │
  │  open stream (yamux SYN)          │
  │──────────────────────────────────►│
  │                                   │
  │  write conn_id (16 bytes)         │
  │──────────────────────────────────►│
  │                                   │  read exactly 16 bytes
  │                                   │  conn_id = Uuid::from_bytes(id_bytes)
  │                                   │  send (conn_id, stream) → main loop
  │                                   │
  ├──── proxy data flows both ways ───┤
```

### Server-side duplex pipe

The server does not connect the yamux `Connection` directly to the data WebSocket socket in the session task. Instead, it uses an **in-process loopback pipe**:

```
Session task                   Yamux driver task
  │                                   │
  │  tokio::io::duplex(64 KiB)        │
  │  server_side ↔ client_side        │
  │                                   │
  │  yamux::Connection(server_side)   │  ◄── drives IO
  │                                   │
  │  pipe_client (taken by bridge)    │
  │           │                       │
  │   ┌───────▼────────────────────┐  │
  │   │ copy_bidirectional         │  │
  │   │  pipe_client ↔ data WS     │  │
  │   └────────────────────────────┘  │
```

`copy_bidirectional` bridges `pipe_client` ↔ the data WebSocket transport. The yamux `Connection` reads/writes its internal framing through `server_side`. This decouples session lifecycle from WebSocket I/O.

---

## Per-Connection Flow

Full end-to-end trace for a single HTTP request through the tunnel:

```
Browser                   Server (HTTP edge)        TunnelCore        Session task        Yamux driver        Client (main loop)     Client (proxy task)     Local service
  │                              │                      │                  │                    │                      │                       │                    │
  │──── GET /api/v1 ────────────►│                      │                  │                    │                      │                       │                    │
  │     Host: myapp.example.com  │                      │                  │                    │                      │                       │                    │
  │                              │──── lookup ──────────►│                  │                    │                      │                       │                    │
  │                              │◄─── TunnelInfo ───────│                  │                    │                      │                       │                    │
  │                              │                      │                  │                    │                      │                       │                    │
  │                              │──── alloc conn_id ───►│                  │                    │                      │                       │                    │
  │                              │    store oneshot_tx   │                  │                    │                      │                       │                    │
  │                              │                      │                  │                    │                      │                       │                    │
  │                              │──── send NewConnection control frame ───►│                    │                      │                       │                    │
  │                              │                      │                  │                    │                      │                       │                    │
  │                              │                      │                  │──── open stream ───►│                      │                       │                    │
  │                              │                      │                  │    (yamux SYN)      │                      │                       │                    │
  │                              │                      │                  │──── write conn_id ──►│                      │                       │                    │
  │                              │                      │                  │    (16 bytes)       │                      │                       │                    │
  │                              │                      │                  │                    │                      │                       │                    │
  │                              │                      │                  │──── resolve_pending_conn ──────────────────►│                       │                    │
  │                              │                      │                  │    (deliver stream via oneshot)             │  NewConnection frame  │                    │
  │                              │                      │                  │                    │◄─ stream arrives ──────│                       │                    │
  │                              │                      │                  │                    │  read 16-byte conn_id  │                       │                    │
  │                              │                      │                  │                    │  send (conn_id, stream)►│                       │                    │
  │                              │                      │                  │                    │                      │──── spawn proxy task ──►│                    │
  │                              │◄──────────── yamux stream ─────────────────────────────────────────────────────────────────────────────────│                    │
  │                              │ (bidirectional copy: edge ↔ yamux stream ↔ proxy task)                                                      │──── TCP connect ───►│
  │                              │                      │                  │                    │                      │                       │                    │
  │◄──── 200 OK ─────────────────│◄═══════════════════════════════════ bytes flow ══════════════════════════════════════════════════════════════►│                    │
```

---

## Concurrency Model

rustunnel uses **Tokio**'s multi-threaded async runtime. All I/O is non-blocking. The key concurrency units are:

### Server-side tasks

| Task | Lifetime | Purpose |
|------|----------|---------|
| Control-plane listener | Server lifetime | Accepts new `/_control` WebSocket connections |
| Session task | Per client session | Runs the control loop for one client |
| Yamux driver task | Per client session | Drives yamux IO, opens streams for new connections |
| Data WebSocket bridge | Per client session | `copy_bidirectional(pipe_client ↔ data_ws)` |
| Edge proxy connection | Per proxied connection | `copy_bidirectional(edge_socket ↔ yamux_stream)` |
| HTTP edge | Server lifetime | Accepts HTTP/HTTPS connections |
| TCP edge listener | Per TCP tunnel | Accepts TCP connections |
| Dashboard | Server lifetime | Serves REST API and SPA |
| Metrics | Server lifetime | Serves Prometheus endpoint |
| ACME renewal | Server lifetime | Background cert renewal |

### Client-side tasks

| Task | Lifetime | Purpose |
|------|----------|---------|
| Main loop | Session lifetime | Control protocol, signal handling, heartbeat |
| Yamux driver | Session lifetime | Accepts inbound yamux streams, reads conn_id |
| Proxy task | Per proxied connection | `copy_bidirectional(yamux_stream ↔ local_tcp)` |

### Shared state (server)

All server tasks share `Arc<TunnelCore>` which uses lock-free interior mutability:

| Field | Type | Purpose |
|-------|------|---------|
| `http_routes` | `DashMap<String, TunnelInfo>` | subdomain → tunnel |
| `tcp_routes` | `DashMap<u16, TunnelInfo>` | port → tunnel |
| `sessions` | `DashMap<Uuid, SessionInfo>` | session_id → session |
| `pending_conns` | `DashMap<Uuid, oneshot::Sender<YamuxStream>>` | conn_id → stream rendezvous |
| `available_tcp_ports` | `Mutex<Vec<u16>>` | port pool |

---

## TLS and Security

### Certificate management

Two modes are supported:

| Mode | Config | Description |
|------|--------|-------------|
| Static PEM | `cert_path` + `key_path` | Pre-existing certificate. Loaded at startup. No auto-renewal. |
| ACME | `acme_enabled = true` | Automatic certificate issuance and renewal via Let's Encrypt. Certificate hot-swapped without restart. |

### Authentication

1. The client sends an `Auth` frame with a **bearer token**.
2. The server validates the token against its SQLite database.
3. A failed auth returns `AuthError` and closes the connection. Auth errors are **fatal** on the client — reconnect is not attempted.

### `--insecure` flag

When `--insecure` is set, the client installs a custom `ServerCertVerifier` that accepts any certificate. This is intended for local development with self-signed certs only. **Never use in production.**

### Rate limiting

Two independent rate limiters run on the server:

| Limiter | Scope | Algorithm |
|---------|-------|-----------|
| IP rate limiter | Per source IP | Sliding window (requests per second) |
| Tunnel rate limiter | Per tunnel | Token bucket |

Both are enforced in the HTTP edge before the request is forwarded.

---

## Metrics and Observability

### Prometheus

The server exposes metrics at `:9090/metrics` in the standard text format:

```
# HELP rustunnel_active_sessions Number of active client sessions
# TYPE rustunnel_active_sessions gauge
rustunnel_active_sessions 3
# HELP rustunnel_active_tunnels_http Number of active HTTP tunnels
# TYPE rustunnel_active_tunnels_http gauge
rustunnel_active_tunnels_http 5
# HELP rustunnel_active_tunnels_tcp Number of active TCP tunnels
# TYPE rustunnel_active_tunnels_tcp gauge
rustunnel_active_tunnels_tcp 2
```

### Structured logging

Both server and client use `tracing` with configurable output. The server supports two formats:

| Format | Config | Use case |
|--------|--------|----------|
| pretty | `format = "text"` | Human-readable terminal output |
| JSON | `format = "json"` | Machine-readable log aggregation (e.g. Loki) |

Log level is controlled by `RUST_LOG` (client) or the `logging.level` config key (server).

### Audit log

The server writes append-only JSON audit events to a configurable file:
- Token creation / revocation
- Session connect / disconnect
- Tunnel register / unregister

---

## Component Dependency Graph

```
rustunnel-client
├── rustunnel-protocol   (control frame types)
├── tokio                (async runtime)
├── tokio-tungstenite    (WebSocket client)
├── yamux 0.13           (stream multiplexer)
├── rustls               (TLS — ring provider)
├── clap                 (CLI)
├── serde-json           (frame serialization)
├── indicatif            (spinner / progress bar)
├── console              (terminal colors)
└── tracing              (structured logging)

rustunnel-server
├── rustunnel-protocol   (control frame types)
├── tokio                (async runtime, multi-thread)
├── axum                 (HTTP edge + dashboard API)
├── tokio-tungstenite    (WebSocket server)
├── yamux 0.13           (stream multiplexer)
├── rustls               (TLS — ring provider + ACME)
├── sqlx + SQLite        (token storage + audit)
├── dashmap              (lock-free routing table)
├── parking_lot          (port pool mutex)
├── clap                 (CLI)
├── serde-json           (frame + REST serialization)
└── tracing              (structured logging)

rustunnel-protocol  (shared library crate)
├── serde + serde-json   (frame serialization)
└── uuid                 (conn_id / session_id / tunnel_id)
```

---

## Crate Structure

```
rustunnel/
├── Cargo.toml                  (workspace)
├── Makefile
├── deploy/
│   ├── Dockerfile
│   ├── docker-compose.yml
│   └── rustunnel.service       (systemd unit)
├── dashboard-ui/               (React SPA, embedded into server binary)
├── docs/
│   ├── client-guide.md
│   └── architecture.md
├── tests/
│   ├── common/mod.rs           (test helpers: test server, connect_data_bridge)
│   └── integration/
│       ├── http_tunnel.rs
│       ├── tcp_tunnel.rs
│       └── reconnect.rs
└── crates/
    ├── rustunnel-protocol/     (shared frame types)
    │   └── src/
    │       ├── frame.rs        (ControlFrame enum, encode/decode)
    │       └── error.rs
    ├── rustunnel-client/
    │   └── src/
    │       ├── main.rs         (CLI definition + entry point)
    │       ├── config.rs       (ClientConfig, TunnelDef, YAML loading)
    │       ├── control.rs      (connect(), main_loop(), drive_client_mux())
    │       ├── reconnect.rs    (exponential backoff retry loop)
    │       ├── proxy.rs        (proxy_connection() — local TCP bridge)
    │       ├── display.rs      (spinner, startup box, request log)
    │       └── error.rs        (Error enum)
    └── rustunnel-server/
        └── src/
            ├── main.rs         (entry point, subsystem wiring)
            ├── config.rs       (ServerConfig, TOML loading)
            ├── lib.rs
            ├── error.rs
            ├── net.rs          (TLS listener helpers)
            ├── audit.rs        (append-only audit log writer)
            ├── control/
            │   ├── server.rs   (run_control_plane — WS upgrade + routing)
            │   ├── session.rs  (run_session — per-client control loop)
            │   └── mux.rs      (WsCompat, MuxSession)
            ├── core/
            │   ├── router.rs   (TunnelCore — central routing table)
            │   ├── tunnel.rs   (TunnelInfo, SessionInfo, ControlMessage)
            │   ├── limiter.rs  (token-bucket rate limiter)
            │   └── ip_limiter.rs (sliding-window IP rate limiter)
            ├── edge/
            │   ├── http.rs     (HTTP + HTTPS reverse proxy)
            │   ├── tcp.rs      (TCP edge listeners)
            │   └── capture.rs  (request capture for dashboard)
            ├── dashboard/
            │   ├── mod.rs      (run_dashboard)
            │   ├── api.rs      (REST endpoints)
            │   ├── ui.rs       (embedded SPA serving)
            │   └── capture.rs  (SSE stream for live request feed)
            ├── db/
            │   ├── mod.rs      (SQLite pool init)
            │   └── models.rs   (Token model + queries)
            └── tls/
                ├── mod.rs      (CertManager — static PEM or ACME)
                └── acme.rs     (ACME challenge + renewal)
```
