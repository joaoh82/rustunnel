use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("tunnel error: {0}")]
    Tunnel(String),

    #[error("no TCP ports available")]
    NoPortsAvailable,

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("tunnel not found: {0}")]
    TunnelNotFound(String),

    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(#[from] rustunnel_protocol::Error),

    #[error("mux error: {0}")]
    Mux(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
