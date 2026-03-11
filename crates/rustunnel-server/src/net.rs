//! Socket helpers — create TCP listeners with low-level socket options set
//! before binding, so the options take effect even on the very first accept.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::TcpListener;

/// Bind a `TcpListener` with `SO_REUSEADDR` (and `SO_REUSEPORT` on Linux)
/// already enabled.  This lets the server restart quickly without waiting for
/// the OS TIME_WAIT window to expire.
pub fn bind_reuse(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    socket.set_reuse_address(true)?;

    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;

    socket.set_nonblocking(true)?;
    socket.bind(&SockAddr::from(addr))?;
    socket.listen(1024)?;

    TcpListener::from_std(socket.into())
}
