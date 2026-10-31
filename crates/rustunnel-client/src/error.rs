//! Client-side error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("auth failed: {0}")]
    Auth(String),

    #[error("tunnel error: {0}")]
    Tunnel(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("protocol error: {0}")]
    Protocol(#[from] rustunnel_protocol::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Stable machine-readable error code (snake_case variant name), used as
    /// the `code` field of the `--json` error event.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Config(_) => "config",
            Error::Auth(_) => "auth",
            Error::Tunnel(_) => "tunnel",
            Error::Connection(_) => "connection",
            Error::Protocol(_) => "protocol",
            Error::Io(_) => "io",
        }
    }

    /// One-line recovery hint for agents/users, when one exists for the
    /// error category. Emitted as the `hint` field of the `--json` error event.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Error::Config(_) => Some(
                "run `rustunnel setup` to create ~/.rustunnel/config.yml, \
                 or pass --server <host:port> and --token <token> directly",
            ),
            // Auth failures from `token create` use the dashboard admin
            // token (--admin-token), not the tunnel auth token — the message
            // mentions "admin token", so key the hint off that.
            Error::Auth(msg) if msg.contains("admin token") => Some(
                "pass the server's admin token with --admin-token \
                 (set as [auth] admin_token in the server config)",
            ),
            Error::Auth(_) => Some(
                "pass a valid token with --token or the RUSTUNNEL_TOKEN env var; \
                 get one at https://rustunnel.com (Dashboard -> API Keys), or for a \
                 self-hosted server create one with `rustunnel token create`",
            ),
            Error::Connection(_) => Some(
                "check network connectivity and the server address; \
                 pass --server <host:port> or --region <eu|us|ap> to pick a different edge",
            ),
            // The server message is already the whole ask, including the
            // upgrade URL. A second hint would talk over it.
            Error::Tunnel(msg) if is_hobby_limit_message(msg) => None,
            Error::Tunnel(_) => Some(
                "the server rejected the tunnel registration; \
                 try a different --subdomain or check the server-side limits",
            ),
            Error::Protocol(_) | Error::Io(_) => None,
        }
    }
}

impl Error {
    /// Text for stderr. Hobby-limit messages from the server are already a
    /// complete sentence plus an upgrade URL. Released clients wrap them as
    /// `error: tunnel error: ...`; this build prints the server text alone.
    pub fn rendered(&self) -> String {
        match self {
            Error::Tunnel(msg) if is_hobby_limit_message(msg) => msg.clone(),
            other => format!("error: {other}"),
        }
    }

    /// `message` field of the `--json` error event. Same rule as
    /// [`Self::rendered`], without the `error:` prefix other variants keep
    /// via [`Display`](std::fmt::Display).
    pub fn public_message(&self) -> String {
        match self {
            Error::Tunnel(msg) if is_hobby_limit_message(msg) => msg.clone(),
            other => other.to_string(),
        }
    }
}

/// True when `msg` is the Hobby upgrade copy the edge puts in `TunnelError`.
pub fn is_hobby_limit_message(msg: &str) -> bool {
    msg.starts_with("Hobby plan")
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_hint_points_at_admin_token_for_token_create_failures() {
        // Message produced by `token create` on a 401/403 mentions "admin token".
        let e = Error::Auth(
            "token creation rejected by localhost:4040 (401): denied — \
             pass a valid --admin-token (the server's admin token)"
                .into(),
        );
        let hint = e.hint().unwrap();
        assert!(hint.contains("--admin-token"));
        assert!(!hint.contains("--token "));
    }

    #[test]
    fn auth_hint_points_at_token_for_tunnel_auth_failures() {
        let e = Error::Auth("server eu.edge.rustunnel.com:4040 rejected authentication".into());
        let hint = e.hint().unwrap();
        assert!(hint.contains("--token"));
        assert!(hint.contains("RUSTUNNEL_TOKEN"));
    }

    #[test]
    fn hobby_limit_is_printed_without_an_error_wrapper() {
        let msg = "Hobby plan limit: 2 concurrent tunnels. Pay-as-you-go removes the cap ($3/mo min).\nUpgrade: https://rustunnel.com/dashboard/billing?upgrade=1&utm_source=cli&utm_campaign=hobby_limit&utm_content=tunnel_limit";
        let e = Error::Tunnel(msg.into());
        // Released CLIs format the same value as `error: tunnel error: ...`.
        assert_eq!(format!("error: {e}"), format!("error: tunnel error: {msg}"));
        assert_eq!(e.rendered(), msg);
        assert_eq!(e.public_message(), msg);
        assert!(e.hint().is_none());
        assert!(!e.rendered().contains("error: tunnel error:"));
    }

    #[test]
    fn other_tunnel_errors_keep_the_existing_prefix() {
        let e = Error::Tunnel("subdomain 'x' is already in use".into());
        assert_eq!(
            e.rendered(),
            "error: tunnel error: subdomain 'x' is already in use"
        );
        assert_eq!(
            e.public_message(),
            "tunnel error: subdomain 'x' is already in use"
        );
        assert!(e.hint().is_some());
    }
}
