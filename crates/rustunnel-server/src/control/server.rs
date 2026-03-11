//! Control-plane TCP listener.
//!
//! Accepts TCP connections, upgrades to TLS (tokio-rustls), then upgrades to
//! WebSocket (tokio-tungstenite), and spawns a session handler for each.

use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_async;

use rustls::ServerConfig as RustlsConfig;

use crate::audit::AuditTx;
use crate::config::ServerConfig;
use crate::control::session::handle_session;
use crate::core::TunnelCore;
use crate::error::Result;
use crate::net::bind_reuse;

/// Start the control-plane listener.
///
/// Binds `addr`, wraps accepted sockets in TLS (using the hot-swappable
/// `tls_config` handle), upgrades them to WebSocket, and spawns a
/// `handle_session` task for each connection.
///
/// `tls_config` is read on **every** inbound connection so that certificate
/// renewals performed by [`crate::tls::CertManager`] are picked up
/// immediately without restarting.
pub async fn run_control_plane(
    addr: SocketAddr,
    core: Arc<TunnelCore>,
    config: Arc<ServerConfig>,
    tls_config: Arc<ArcSwap<RustlsConfig>>,
    audit_tx: AuditTx,
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

        tracing::debug!(%peer_addr, "new TCP connection");

        // Per-connection: snapshot the current rustls config so renewed certs
        // take effect on the very next connection.
        let acceptor = TlsAcceptor::from(Arc::clone(&tls_config.load()));
        let core = core.clone();
        let config = config.clone();
        let audit_tx = audit_tx.clone();

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

            // WebSocket upgrade.
            let ws_stream = match accept_async(tls_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%peer_addr, "WebSocket upgrade failed: {e}");
                    return;
                }
            };
            tracing::debug!(%peer_addr, "WebSocket upgrade complete");

            handle_session(ws_stream, peer_addr, core, config, audit_tx).await;
        });
    }
}
