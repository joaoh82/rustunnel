# Latency Investigation — Why Tunnel Requests Are Slow

**Status**: Investigation complete, fixes not yet implemented
**Context**: Server on Hetzner, DNS via Cloudflare, comparing against ngrok

---

## Summary

There are **6 distinct latency sources** found in the codebase. They stack on top of each other, which explains why the difference feels pronounced. The two biggest are entirely fixable in code without infrastructure changes.

---

## How a Request Currently Travels (Critical Path)

Before listing problems, here is the exact sequence every HTTP request goes through today. Each arrow that crosses the network is a latency contribution.

```
Browser
  │
  │  HTTPS (Cloudflare or direct)
  ▼
Hetzner server — HTTP edge (port 443)
  │
  │  1. Lookup subdomain in TunnelCore.http_routes  [in-memory, ~0ms]
  │  2. Register oneshot in pending_conns           [in-memory, ~0ms]
  │  3. Send NewConnection → mpsc channel           [in-process, ~0ms]
  │
  ▼  NewConnection control frame
  Session task (same server, different async task)
  │
  │  4. open_tx.send(conn_id)   → yamux driver task [in-process, ~0ms]
  │  5. send_frame(NewConnection) → control WS      [network write]
  │
  ▼  control WebSocket (TLS, ~40-200ms RTT depending on location)
  rustunnel client (your laptop)
  │
  │  6. main_loop receives NewConnection
  │     → stores in pending_conns map
  │
  ▼  Meanwhile, on the server's yamux driver:
  │  7. poll_new_outbound() opens yamux stream
  │  8. write_all(conn_id 16 bytes) + flush()
  │
  ▼  data WebSocket (TLS, same RTT)
  Client yamux driver
  │
  │  9. poll_next_inbound() accepts stream
  │  10. read_exact(16 bytes) to get conn_id
  │  11. stream_tx.send((conn_id, stream)) → main_loop
  │
  ▼
  Client main_loop
  │
  │  12. Match stream + pending_conn → spawn proxy task
  │
  ▼
  Proxy task
  │
  │  13. TcpStream::connect("localhost:port")  [new TCP connection]
  │  14. hyper HTTP/1.1 handshake             [new HTTP handshake]
  │  15. send_request(req)
  │  16. collect() entire response body       [full buffering]
  │
  ▼
  Local service → response flows back the same path
```

**ngrok comparison**: ngrok pre-opens a pool of multiplexed streams and uses a single coordinated signaling step. Steps 4–12 above collapse into roughly one message hop.

---

## Issue 1 — A New yamux Stream Is Opened Per Request (Highest Impact)

**File**: `crates/rustunnel-server/src/control/session.rs:130`, `crates/rustunnel-client/src/control.rs:510`

Every single HTTP request triggers:
- A new yamux `SYN` frame from server → client on the data WebSocket
- The client reading a 16-byte `conn_id` prefix from the new stream
- An async channel message from `drive_client_mux` → `main_loop`
- A map lookup to correlate the stream with the pending `NewConnection`

That is 4 coordination steps, each requiring the yamux connection to be polled and the data WebSocket to carry frames. At 40–200ms RTT to Hetzner, this alone adds 40–200ms to every request's setup time, before any application bytes are sent.

**ngrok** maintains a pre-opened pool of multiplexed streams (typically 4–8). When a request arrives, a stream is grabbed immediately from the pool, and the pool is refilled in the background.

**Fix**: Implement a stream pre-open pool on the server side. The yamux driver task should maintain N idle streams. When a `NewConnection` arrives, one is taken from the pool immediately rather than opening a new one. The pool is then refilled asynchronously.

---

## Issue 2 — No TCP_NODELAY on the Control/Data WebSocket Connections (High Impact)

**File**: `crates/rustunnel-server/src/control/server.rs:65` (TLS accept path)

The HTTP edge sets `tcp.set_nodelay(true)` on line 210 of `edge/http.rs` — correctly. But the **control-plane TCP socket** and **data WebSocket TCP socket** do not set `TCP_NODELAY`. This means Nagle's algorithm is active.

Nagle's algorithm buffers small writes for up to **40ms** waiting to coalesce them with acknowledgement data. The control channel carries exclusively small JSON frames (~100–200 bytes). Every control frame — including `NewConnection` — can be delayed up to 40ms by the kernel before being put on the wire.

```rust
// server.rs — TLS accept path, before acceptor.accept(tcp_stream)
// Missing:
let _ = tcp_stream.set_nodelay(true);
```

This is a one-line fix with no architectural change required.

---

## Issue 3 — Full Response Body Is Buffered Before Sending (High Impact for Non-Trivial Responses)

**File**: `crates/rustunnel-server/src/edge/http.rs:418`

```rust
// Current code — entire body collected before the response is sent
let body_bytes = resp_body.collect().await?.to_bytes();
```

This means: for a 500 KB HTML page, the server waits until every last byte is received from the local service before sending the first byte to the browser. Time-to-first-byte (TTFB) becomes total-transfer-time. For any response larger than a few kilobytes, this doubles the perceived load time.

The root cause is that `forward_http` returns `Response<BoxBody>` where the body must be fully owned. Streaming would require returning a body that wraps the yamux stream directly.

**Fix**: Change `forward_http` to return a streaming body using `http_body_util::StreamBody` wrapping the yamux stream's frames, rather than collecting all bytes upfront.

---

## Issue 4 — The Duplex Pipe Is an Extra In-Memory Copy Hop (Medium Impact)

**File**: `crates/rustunnel-server/src/control/session.rs:102–108`, `crates/rustunnel-server/src/control/mux.rs:154`

The server-side yamux `Connection` is backed by `tokio::io::duplex`, not directly by the data WebSocket:

```
Data WebSocket → WsCompat → copy_bidirectional → pipe_client ↔ pipe_server → yamux Connection
```

Every byte in the data plane passes through two extra async copies: once into the pipe and once out of it. The `tokio::io::duplex` pair has a 64 KB buffer, which means flows larger than 64 KB stall while waiting for the buffer to drain.

This architecture was introduced to decouple session lifetime from data WebSocket arrival timing. That problem is real, but the duplex pipe is a blunt solution.

**Fix**: Store the data WebSocket directly in the yamux `Connection` using an `Option`. When the data WebSocket arrives, swap it in atomically via `Arc<Mutex<Option<...>>>`. This eliminates the extra copy hop and the 64 KB buffer bottleneck.

---

## Issue 5 — New HTTP/1.1 Handshake to the Local Service Per Request (Medium Impact)

**File**: `crates/rustunnel-server/src/edge/http.rs:399–407`

```rust
let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
    .handshake(io)
    .await?;
```

Every proxied request performs a fresh HTTP/1.1 handshake with the local service over the newly opened yamux stream. There is no connection pooling or keep-alive reuse. Since the local service is on localhost (no network RTT), this cost is small (~1ms) but it is not zero, and it adds a Tokio task per connection (`tokio::spawn` for `conn.with_upgrades()`).

This is lower priority than issues 1–4 because the local TCP connection to `localhost` is essentially free. The overhead is the async task spawn and the HTTP handshake itself.

---

## Issue 6 — yamux Driver Serialises Stream Open and IO Polling (Medium Impact)

**File**: `crates/rustunnel-server/src/control/session.rs:127–162`

```rust
tokio::select! {
    req = open_rx.recv() => {
        // open stream, write 16 bytes, flush — blocks this arm
        poll_new_outbound → write_all → flush → resolve_pending_conn
    }
    result = poll_fn(|cx| conn.poll_next_inbound(cx)) => { ... }
}
```

The `select!` is biased: while the driver is inside the `open_rx` arm (opening a stream, writing, flushing), `poll_next_inbound` is not being called. This means:

- yamux flow-control `WINDOW_UPDATE` frames from the client are not processed until after the write completes
- Under concurrent requests, subsequent `open_rx` messages queue up behind the current write

When multiple requests arrive simultaneously (e.g. browser loading 6 subresources in parallel), they are served one at a time by this driver.

**Fix**: Decouple stream opening from IO polling. Use a dedicated task for each stream open/write, leaving the main driver loop free to continuously call `poll_next_inbound`.

---

## Infrastructure Factors (Not Code)

### Geographic RTT to Hetzner

The round-trip time from the client's machine to Hetzner varies:

| Client location | Approximate RTT |
|----------------|-----------------|
| Frankfurt area | ~10ms |
| London | ~20ms |
| New York | ~90ms |
| São Paulo | ~190ms |
| Singapore | ~170ms |

Since the new-stream setup (Issue 1) adds approximately 1 RTT per request, and the control frame (Issue 2) can add 40ms of Nagle buffering on top, a user in São Paulo sees ~230ms added to *every click* before the request even reaches the local service.

### Cloudflare Proxy Mode (Orange Cloud)

If the DNS record is set to **Proxied** (orange cloud), Cloudflare inserts itself between the browser and Hetzner:

```
Browser → Cloudflare PoP (nearest) → Hetzner → tunnel → local service
```

**This is actually neutral-to-beneficial** for the browser-to-Hetzner leg because Cloudflare terminates TLS at the nearest PoP. However, it adds Cloudflare's own processing time (~5–15ms) and potentially changes which TCP optimizations are in effect.

For the highest raw performance, use **DNS-only mode** (grey cloud) — the browser connects directly to Hetzner's IP. This removes the Cloudflare hop entirely.

If you need Cloudflare's DDoS protection, staying on proxy mode is fine — the code-level fixes (Issues 1–4) will have far more impact.

### Hetzner Location

If the server is in Nuremberg/Frankfurt and the client is in the same continent, RTT is low enough that Issues 1 and 2 are manageable (~50ms overhead per request). If the client is in the Americas or Asia-Pacific, Issues 1 and 2 become the dominant cost.

---

## Fix Priority Matrix

| Issue | Impact | Effort | Change scope |
|-------|--------|--------|-------------|
| 1. New stream per request → pre-open pool | Very High | Medium | `session.rs`, `control.rs` |
| 2. Missing `TCP_NODELAY` on WS connections | High | Trivial | `server.rs` (1 line) |
| 3. Full body buffering before send | High | Medium | `http.rs` |
| 4. Duplex pipe extra copy | Medium | High | `mux.rs`, `session.rs` |
| 5. No HTTP keep-alive to local service | Low | Medium | `http.rs` |
| 6. yamux driver serialisation | Medium | Medium | `session.rs` |

---

## Recommended Fix Order

**Phase 1 — Quick wins** (1–2 hours, no architectural change)
1. Add `set_nodelay(true)` on the control/data WebSocket TCP sockets — Issue 2
2. Remove full body buffering, stream the response body — Issue 3

**Phase 2 — Core protocol improvement** (1–2 days, architectural)
3. Implement a pre-opened yamux stream pool — Issue 1
   This is the single biggest improvement. A pool of 4 streams eliminates the stream-setup RTT entirely for the common case.

**Phase 3 — Data path cleanup** (1 day)
4. Replace the duplex pipe with direct data WebSocket backing — Issue 4
5. Decouple yamux driver stream-open from IO polling — Issue 6

---

## How to Measure

Before and after any fix, measure with:

```bash
# Time to first byte through the tunnel
curl -o /dev/null -s -w "TTFB: %{time_starttransfer}s  Total: %{time_total}s\n" \
  https://yoursubdomain.tunnel.example.com/

# Compare against direct localhost
curl -o /dev/null -s -w "TTFB: %{time_starttransfer}s  Total: %{time_total}s\n" \
  http://localhost:3000/

# Measure control-plane RTT separately
RUST_LOG=rustunnel=debug rustunnel http 3000 2>&1 | grep "pong"
# The Ping→Pong round-trip is logged at debug level; this gives you raw RTT
```

Enable server-side timing logs:

```toml
# server.toml
[logging]
level = "debug"
```

Then look for `request complete duration_ms=` in the server logs — that is the total time the server spent on one request, including the stream-setup wait.
