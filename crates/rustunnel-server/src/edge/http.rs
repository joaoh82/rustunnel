//! HTTP / HTTPS edge proxy.
//!
//! * Port 80  — plain HTTP, every request → 301 redirect to HTTPS.
//! * Port 443 — TLS-terminated; requests are proxied through the tunnel
//!              identified by the `Host` subdomain.
//!
//! Proxy flow for a normal request
//! ────────────────────────────────
//! 1. Parse `Host` header → extract subdomain.
//! 2. `core.resolve_http(subdomain)` → (TunnelInfo, control_tx).
//! 3. Generate `conn_id`; register a pending-stream oneshot in `core`.
//! 4. Send `ControlMessage::NewConnection` to the session.
//! 5. Wait ≤ STREAM_TIMEOUT for the client to open a yamux data stream.
//! 6. Forward the HTTP request through a hyper/h1 client over the stream.
//! 7. Stream the response back to the public caller.
//! 8. Emit a `CaptureEvent` for the dashboard.
//!
//! WebSocket upgrade
//! ──────────────────
//! Detected via `Upgrade: websocket`; after the 101 response the upgraded
//! connection is bridged to the yamux stream via copy_bidirectional.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Body, Incoming};
use hyper::header::HOST;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info, warn};
use uuid::Uuid;
use yamux::Stream as YamuxStream;

use rustunnel_protocol::TunnelProtocol;

use crate::core::{ControlMessage, TunnelCore};
use crate::edge::capture::{CaptureEvent, CaptureTx};
use crate::net::bind_reuse;

// ── timeouts ──────────────────────────────────────────────────────────────────

const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
const PROXY_TIMEOUT: Duration = Duration::from_secs(60);

// ── body type ─────────────────────────────────────────────────────────────────

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full(b: impl Into<Bytes>) -> BoxBody {
    Full::new(b.into()).map_err(|e| match e {}).boxed()
}

fn empty() -> BoxBody {
    Empty::<Bytes>::new().map_err(|e| match e {}).boxed()
}

// ── shared context ────────────────────────────────────────────────────────────

/// Runtime limits passed through the edge proxy hot-path.
#[derive(Clone)]
pub struct HttpEdgeConfig {
    pub rate_limit_rps: u32,
    pub request_body_max_bytes: usize,
}

#[derive(Clone)]
struct ProxyCtx {
    core: Arc<TunnelCore>,
    capture_tx: Option<CaptureTx>,
    domain: String,
    rate_limit_rps: u32,
    request_body_max_bytes: usize,
}

// ── public entry point ────────────────────────────────────────────────────────

/// Start the HTTP (redirect) and HTTPS (proxy) edge listeners concurrently.
pub async fn run_http_edge(
    http_addr: SocketAddr,
    https_addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
    core: Arc<TunnelCore>,
    domain: String,
    capture_tx: Option<CaptureTx>,
    limits: HttpEdgeConfig,
) -> crate::error::Result<()> {
    let ctx = ProxyCtx {
        core,
        capture_tx,
        domain,
        rate_limit_rps: limits.rate_limit_rps,
        request_body_max_bytes: limits.request_body_max_bytes,
    };

    tokio::select! {
        r = run_http_redirect(http_addr, ctx.domain.clone()) => r,
        r = run_https_proxy(https_addr, tls_config, ctx)    => r,
    }
}

// ── HTTP redirect (port 80) ───────────────────────────────────────────────────

async fn run_http_redirect(addr: SocketAddr, domain: String) -> crate::error::Result<()> {
    let listener = bind_reuse(addr)?;
    info!(%addr, "HTTP redirect listener ready");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!("accept error: {e}");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let domain = domain.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(tcp);
            let svc = service_fn(move |req: Request<Incoming>| {
                let domain = domain.clone();
                async move { Ok::<_, Infallible>(redirect_to_https(req, &domain)) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await
            {
                debug!(%peer, "HTTP redirect error: {e}");
            }
        });
    }
}

fn redirect_to_https(req: Request<Incoming>, domain: &str) -> Response<BoxBody> {
    // Sanitise the Host header to prevent header injection: only allow chars
    // that are valid in a hostname or port (alphanumeric, hyphens, dots, colon).
    let raw_host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(domain);
    let host = sanitize_host(raw_host).unwrap_or_else(|| domain.to_string());

    let pq = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let location = format!("https://{host}{pq}");
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("Location", location)
        .body(empty())
        .unwrap()
}

/// Return `Some(host)` when the value contains only safe hostname characters,
/// or `None` when it looks like an injection attempt.
fn sanitize_host(host: &str) -> Option<String> {
    // Strip trailing port if present: "example.com:8080" → "example.com"
    let (name, port_part) = match host.rfind(':') {
        Some(pos) => (&host[..pos], Some(&host[pos..])),
        None => (host, None),
    };
    // Validate hostname part: alphanumeric, hyphens, and dots only.
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !name.is_empty()
    {
        // Validate port part if present: colon + digits.
        if let Some(port) = port_part {
            if port.len() > 1 && port[1..].chars().all(|c| c.is_ascii_digit()) {
                return Some(host.to_string());
            }
            return None; // invalid port
        }
        Some(host.to_string())
    } else {
        None
    }
}

// ── HTTPS proxy (port 443) ────────────────────────────────────────────────────

async fn run_https_proxy(
    addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
    ctx: ProxyCtx,
) -> crate::error::Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let listener = bind_reuse(addr)?;
    info!(%addr, "HTTPS proxy listener ready");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!("accept error: {e}");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let ctx = ctx.clone();

        tokio::spawn(async move {
            let tls = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    debug!(%peer, "TLS failed: {e}");
                    return;
                }
            };
            let io = TokioIo::new(tls);
            let svc = service_fn(move |req: Request<Incoming>| {
                let ctx = ctx.clone();
                async move { Ok::<_, Infallible>(proxy_request(req, peer, ctx).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                debug!(%peer, "HTTPS conn error: {e}");
            }
        });
    }
}

// ── core proxy logic ──────────────────────────────────────────────────────────

async fn proxy_request(
    req: Request<Incoming>,
    peer: SocketAddr,
    ctx: ProxyCtx,
) -> Response<BoxBody> {
    let start = Instant::now();

    // ── 0. IP rate limit ──────────────────────────────────────────────────
    if !ctx.core.ip_limiter.check(peer.ip()) {
        return err_response(StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded");
    }

    // ── 1. Extract subdomain ──────────────────────────────────────────────
    let host = match req.headers().get(HOST).and_then(|v| v.to_str().ok()) {
        Some(h) => h.to_owned(),
        None => return err_response(StatusCode::BAD_REQUEST, "Missing Host header"),
    };
    let subdomain = match extract_subdomain(&host, &ctx.domain) {
        Some(s) => s,
        None => return err_response(StatusCode::BAD_REQUEST, "Cannot parse subdomain"),
    };

    // ── 2. Resolve tunnel ─────────────────────────────────────────────────
    let (tunnel_info, control_tx) = match ctx.core.resolve_http(&subdomain) {
        Some(pair) => pair,
        None => {
            info!(subdomain, "tunnel not found → 502");
            return gateway_error(&subdomain);
        }
    };

    // ── 2a. Per-tunnel rate limit ─────────────────────────────────────────
    if !ctx
        .core
        .rate_limiter
        .check_rate_limit(&tunnel_info.tunnel_id, ctx.rate_limit_rps)
    {
        return err_response(StatusCode::TOO_MANY_REQUESTS, "Tunnel rate limit exceeded");
    }

    // ── 2b. Request body size limit (Content-Length fast-path) ────────────
    if let Some(content_length) = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if content_length > ctx.request_body_max_bytes {
            return err_response(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    }

    // ── 2c. Concurrent connection limit ──────────────────────────────────
    let _permit = match tunnel_info.conn_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many concurrent connections",
            );
        }
    };

    let conn_id = Uuid::new_v4();
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".into());
    let is_ws = is_websocket_upgrade(&req);

    info!(%conn_id, %peer, subdomain, method, path, ws = is_ws, "proxying");

    // ── 3. Register pending stream ────────────────────────────────────────
    let stream_rx = ctx.core.register_pending_conn(conn_id);

    // ── 4. Notify session ─────────────────────────────────────────────────
    if let Err(e) = control_tx
        .send(ControlMessage::NewConnection {
            conn_id,
            client_addr: peer,
            protocol: TunnelProtocol::Http,
        })
        .await
    {
        warn!(%conn_id, "control send failed: {e}");
        ctx.core.cancel_pending_conn(&conn_id);
        return err_response(StatusCode::BAD_GATEWAY, "Tunnel session unavailable");
    }

    // ── 5. Wait for yamux data stream ─────────────────────────────────────
    let yamux_stream = match timeout(STREAM_TIMEOUT, stream_rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            warn!(%conn_id, "pending-conn sender dropped");
            return err_response(StatusCode::BAD_GATEWAY, "Tunnel did not open a data stream");
        }
        Err(_) => {
            warn!(%conn_id, "timed out waiting for data stream");
            ctx.core.cancel_pending_conn(&conn_id);
            return err_response(StatusCode::GATEWAY_TIMEOUT, "Tunnel stream timeout");
        }
    };

    // ── 6. WebSocket upgrade fast-path ────────────────────────────────────
    if is_ws {
        return handle_ws_upgrade(req, yamux_stream, conn_id, &ctx, start).await;
    }

    // ── 7. HTTP proxy ─────────────────────────────────────────────────────
    let request_bytes = req.body().size_hint().upper().unwrap_or(0);

    let resp = match timeout(PROXY_TIMEOUT, forward_http(req, yamux_stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!(%conn_id, "proxy error: {e}");
            return err_response(StatusCode::BAD_GATEWAY, "Proxy error");
        }
        Err(_) => {
            warn!(%conn_id, "proxy timeout");
            return err_response(StatusCode::GATEWAY_TIMEOUT, "Proxy timeout");
        }
    };

    let status = resp.status().as_u16();
    let response_bytes = resp.body().size_hint().upper().unwrap_or(0);
    let duration_ms = start.elapsed().as_millis() as u64;

    info!(%conn_id, subdomain, status, duration_ms, "request complete");

    emit_capture(
        &ctx.capture_tx,
        CaptureEvent {
            conn_id,
            tunnel_id: tunnel_info.tunnel_id,
            tunnel_label: subdomain,
            method,
            path,
            status,
            request_bytes,
            response_bytes,
            duration_ms,
            captured_at: SystemTime::now(),
        },
    );

    resp
}

// ── HTTP forwarding via hyper client ─────────────────────────────────────────

async fn forward_http(
    req: Request<Incoming>,
    yamux_stream: YamuxStream,
) -> Result<Response<BoxBody>, Box<dyn std::error::Error + Send + Sync>> {
    // Bridge yamux (futures::io) → tokio::io → hyper::rt IO.
    let io = TokioIo::new(yamux_stream.compat());

    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .handshake(io)
        .await?;

    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!("upstream conn error: {e}");
        }
    });

    // Strip hop-by-hop headers before forwarding upstream.
    let (mut parts, body) = req.into_parts();
    remove_hop_by_hop(&mut parts.headers);
    let fwd_req = Request::from_parts(parts, body);

    let upstream = sender.send_request(fwd_req).await?;

    // Collect the full response body.
    let (mut resp_parts, resp_body) = upstream.into_parts();
    let body_bytes = resp_body.collect().await?.to_bytes();
    remove_hop_by_hop(&mut resp_parts.headers);

    Ok(Response::from_parts(resp_parts, full(body_bytes)))
}

// ── WebSocket upgrade ─────────────────────────────────────────────────────────

async fn handle_ws_upgrade(
    mut req: Request<Incoming>,
    yamux_stream: YamuxStream,
    conn_id: Uuid,
    ctx: &ProxyCtx,
    start: Instant,
) -> Response<BoxBody> {
    debug!(%conn_id, "WebSocket upgrade");

    let upgrade_fut = hyper::upgrade::on(&mut req);

    tokio::spawn(async move {
        match upgrade_fut.await {
            Err(e) => warn!(%conn_id, "upgrade failed: {e}"),
            Ok(upgraded) => {
                // hyper::upgrade::Upgraded → tokio::io via TokioIo.
                let mut client_io = TokioIo::new(upgraded);
                // yamux::Stream (futures::io) → tokio::io via compat().
                let mut upstream = yamux_stream.compat();
                match tokio::io::copy_bidirectional(&mut client_io, &mut upstream).await {
                    Ok((up, dn)) => debug!(%conn_id, bytes_up=up, bytes_dn=dn, "WS done"),
                    Err(e) => debug!(%conn_id, "WS copy: {e}"),
                }
            }
        }
    });

    emit_capture(
        &ctx.capture_tx,
        CaptureEvent {
            conn_id,
            tunnel_id: Uuid::nil(),
            tunnel_label: String::new(),
            method: "WS-UPGRADE".into(),
            path: String::new(),
            status: 101,
            request_bytes: 0,
            response_bytes: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            captured_at: SystemTime::now(),
        },
    );

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .body(empty())
        .unwrap()
}

// ── helpers ───────────────────────────────────────────────────────────────────

pub fn extract_subdomain(host: &str, domain: &str) -> Option<String> {
    let host = host.split(':').next().unwrap_or(host);
    let suffix = format!(".{domain}");
    if host == domain {
        return None;
    }
    host.strip_suffix(&suffix).map(str::to_string)
}

fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get("Upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

static HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn remove_hop_by_hop(headers: &mut hyper::HeaderMap) {
    for &name in HOP_BY_HOP {
        headers.remove(name);
    }
}

fn err_response(status: StatusCode, msg: &str) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(full(msg.to_string()))
        .unwrap()
}

fn gateway_error(subdomain: &str) -> Response<BoxBody> {
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Tunnel Not Found — rustunnel</title></head>
<body style="font-family:sans-serif;max-width:600px;margin:4rem auto;color:#333">
  <h1>502 Bad Gateway</h1>
  <p>No tunnel is registered for <strong>{subdomain}</strong>.</p>
  <p>Make sure your <code>rustunnel-client</code> is running and authenticated.</p>
</body>
</html>"#
    );
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(full(html))
        .unwrap()
}

fn emit_capture(tx: &Option<CaptureTx>, event: CaptureEvent) {
    if let Some(tx) = tx {
        let _ = tx.try_send(event);
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn subdomain_extraction() {
        assert_eq!(
            extract_subdomain("myapp.tunnel.example.com", "tunnel.example.com"),
            Some("myapp".into())
        );
        assert_eq!(
            extract_subdomain("myapp.tunnel.example.com:443", "tunnel.example.com"),
            Some("myapp".into())
        );
        // Bare domain → None
        assert_eq!(
            extract_subdomain("tunnel.example.com", "tunnel.example.com"),
            None
        );
        // Unrelated domain → None
        assert_eq!(
            extract_subdomain("other.example.com", "tunnel.example.com"),
            None
        );
        // Multi-level subdomain
        assert_eq!(
            extract_subdomain("a.b.tunnel.example.com", "tunnel.example.com"),
            Some("a.b".into())
        );
    }

    #[test]
    fn hop_by_hop_headers_stripped() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("x-request-id", HeaderValue::from_static("abc"));
        remove_hop_by_hop(&mut headers);
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("transfer-encoding"));
        assert!(
            headers.contains_key("x-request-id"),
            "custom headers must survive"
        );
    }

    #[test]
    fn websocket_detection() {
        // Test the header-presence logic directly against a HeaderMap.
        let mut headers = hyper::HeaderMap::new();
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        assert!(headers
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false));

        // No upgrade header → false.
        let empty: hyper::HeaderMap = hyper::HeaderMap::new();
        assert!(!empty
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false));
    }
}
