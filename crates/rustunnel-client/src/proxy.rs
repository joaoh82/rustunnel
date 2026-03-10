//! Local service proxy.
//!
//! `proxy_connection` bridges a yamux data stream (the tunnel side) with a
//! fresh TCP connection to the local service.

use std::net::SocketAddr;
use std::time::Instant;

use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info, warn};
use uuid::Uuid;
use yamux::Stream as YamuxStream;

/// Proxy bytes between `yamux_stream` (tunnel-side) and a new TCP connection
/// to `local_addr` (service-side).
///
/// Logs byte counts and duration on completion.
pub async fn proxy_connection(yamux_stream: YamuxStream, local_addr: SocketAddr, conn_id: Uuid) {
    debug!(%conn_id, %local_addr, "proxy: connecting to local service");

    let mut local = match tokio::net::TcpStream::connect(local_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%conn_id, %local_addr, "proxy: failed to connect to local service: {e}");
            return;
        }
    };

    // yamux::Stream implements futures::io::{AsyncRead, AsyncWrite}.
    // Bridge to tokio IO traits with the compat wrapper.
    let mut remote = yamux_stream.compat();

    let started = Instant::now();

    match tokio::io::copy_bidirectional(&mut local, &mut remote).await {
        Ok((up, down)) => {
            info!(
                %conn_id,
                bytes_to_local   = up,
                bytes_to_tunnel  = down,
                duration_ms      = started.elapsed().as_millis() as u64,
                "proxy: connection done"
            );
        }
        Err(e) => {
            debug!(%conn_id, "proxy: copy error: {e}");
        }
    }
}
