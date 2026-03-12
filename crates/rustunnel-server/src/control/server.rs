//! Control-plane TCP listener.
//!
//! Accepts TCP connections, upgrades to TLS (tokio-rustls), then upgrades to
//! WebSocket (tokio-tungstenite), and routes each connection based on the
//! HTTP request path:
//!
//! * `/_control`            → session handler (auth + tunnel control frames)
//! * `/_data/<session_id>`  → data-plane bridge (yamux frames from the client)

use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use uuid::Uuid;

use rustls::ServerConfig as RustlsConfig;

use crate::audit::AuditTx;
use crate::config::ServerConfig;
use crate::control::session::{handle_data_connection, handle_session};
use crate::core::TunnelCore;
use crate::error::Result;
use crate::net::bind_reuse;
use sqlx::SqlitePool;

/// Start the control-plane listener.
///
/// Binds `addr`, wraps accepted sockets in TLS (using the hot-swappable
/// `tls_config` handle), upgrades them to WebSocket, and dispatches each
/// connection to the correct handler based on the HTTP request path.
///
/// `tls_config` is read on **every** inbound connection so that certificate
/// renewals performed by [`crate::tls::CertManager`] are picked up
/// immediately without restarting.
// The `accept_hdr_async` callback must return `Result<Response, Response>`.
// The `Err` variant (`hyper::Response<Option<String>>`) is a third-party type
// we cannot reduce in size, so the lint is suppressed here.
#[allow(clippy::result_large_err)]
pub async fn run_control_plane(
    addr: SocketAddr,
    core: Arc<TunnelCore>,
    config: Arc<ServerConfig>,
    tls_config: Arc<ArcSwap<RustlsConfig>>,
    audit_tx: AuditTx,
    pool: SqlitePool,
) -> Result<()> {
    let listener = bind_reuse(addr)?;
    tracing::info!(%addr, "control plane listening");

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
        };

        // Disable Nagle's algorithm so small control frames (NewConnection, etc.)
        // are sent immediately rather than buffered for up to 40 ms.
        let _ = tcp_stream.set_nodelay(true);

        tracing::debug!(%peer_addr, "new TCP connection");

        let acceptor = TlsAcceptor::from(Arc::clone(&tls_config.load()));
        let core = core.clone();
        let config = config.clone();
        let audit_tx = audit_tx.clone();
        let pool = pool.clone();

        tokio::spawn(async move {
            // TLS handshake.
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%peer_addr, "TLS handshake failed: {e}");
                    return;
                }
            };
            tracing::debug!(%peer_addr, "TLS handshake complete");

            // WebSocket upgrade — capture the HTTP request path before completing.
            let mut captured_path = String::new();
            let ws_stream = match accept_hdr_async(tls_stream, |req: &Request, resp: Response| {
                captured_path = req.uri().path().to_string();
                Ok(resp)
            })
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%peer_addr, "WebSocket upgrade failed: {e}");
                    return;
                }
            };
            tracing::debug!(%peer_addr, path = %captured_path, "WebSocket upgrade complete");

            // Route based on path.
            if captured_path == "/_control" {
                handle_session(ws_stream, peer_addr, core, config, audit_tx, pool).await;
            } else if let Some(id_str) = captured_path.strip_prefix("/_data/") {
                match Uuid::parse_str(id_str) {
                    Ok(session_id) => {
                        handle_data_connection(ws_stream, session_id, core).await;
                    }
                    Err(_) => {
                        tracing::warn!(%peer_addr, "invalid session_id in data path: {id_str}");
                    }
                }
            } else {
                tracing::warn!(%peer_addr, path = %captured_path, "unknown WebSocket path — closing");
            }
        });
    }
}
