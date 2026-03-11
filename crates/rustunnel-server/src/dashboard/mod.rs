//! Dashboard HTTP server (port 4040 by default).
//!
//! Combines the REST API (`/api/…`) with the embedded SPA.

pub mod api;
pub mod capture;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::HeaderValue;
use axum::Router;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

use crate::audit::AuditTx;
use crate::core::TunnelCore;
use crate::edge::capture::CaptureEvent;
use crate::error::Result;

use api::ApiState;
use capture::start_capture_service;

/// Start the dashboard HTTP server.
///
/// * `addr`        — listen address (e.g. `0.0.0.0:4040`)
/// * `core`        — shared tunnel routing state
/// * `pool`        — SQLite pool (already migrated)
/// * `capture_rx`  — receiver end of the capture channel from the HTTP edge
/// * `admin_token` — admin bearer token from config
/// * `audit_tx`    — audit event sender
pub async fn run_dashboard(
    addr: SocketAddr,
    core: Arc<TunnelCore>,
    pool: SqlitePool,
    capture_rx: mpsc::Receiver<CaptureEvent>,
    admin_token: String,
    audit_tx: AuditTx,
) -> Result<()> {
    let capture_store = start_capture_service(capture_rx, pool.clone());

    let state = ApiState {
        core,
        pool,
        capture: capture_store,
        admin_token,
        audit_tx,
    };

    let app = Router::new()
        .merge(api::router(state))
        .merge(ui::router())
        // Security headers applied to every response.
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'unsafe-inline'; \
                 style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                 connect-src 'self'",
            ),
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "dashboard listening");

    axum::serve(listener, app).await?;
    Ok(())
}
