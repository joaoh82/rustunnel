//! Dashboard HTTP server (port 8443 by default).
//!
//! Serves the REST API (`/api/…`) only.  The dashboard UI is a separate
//! Next.js app deployed on Vercel — it talks to this API over HTTPS with
//! CORS enabled.

pub mod api;
pub mod capture;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, Method};
use axum::Router;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

use crate::audit::AuditTx;
use crate::config::RegionSection;
use crate::core::TunnelCore;
use crate::db::Db;
use crate::edge::capture::CaptureEvent;
use crate::error::Result;

use api::ApiState;
use capture::start_capture_service;

/// Start the dashboard HTTP server.
///
/// * `addr`             — listen address (e.g. `0.0.0.0:8443`)
/// * `core`             — shared tunnel routing state
/// * `db`               — dual-pool database handle (already migrated)
/// * `capture_rx`       — receiver end of the capture channel from the HTTP edge
/// * `admin_token`      — admin bearer token from config
/// * `audit_tx`         — audit event sender
/// * `dashboard_origin` — allowed CORS origin for the external dashboard UI
/// * `region`           — identity of this server instance
#[allow(clippy::too_many_arguments)]
pub async fn run_dashboard(
    addr: SocketAddr,
    core: Arc<TunnelCore>,
    db: Db,
    capture_rx: mpsc::Receiver<CaptureEvent>,
    admin_token: String,
    audit_tx: AuditTx,
    dashboard_origin: String,
    region: RegionSection,
) -> Result<()> {
    let capture_store = start_capture_service(capture_rx, db.local.clone());

    let state = ApiState {
        core,
        db,
        capture: capture_store,
        admin_token,
        audit_tx,
        region,
    };

    // ── CORS ──────────────────────────────────────────────────────────────────
    // Allow the configured dashboard origin plus localhost:3000 for local dev.
    let mut origins: Vec<HeaderValue> = Vec::new();
    if let Ok(v) = dashboard_origin.parse::<HeaderValue>() {
        origins.push(v);
    }
    let localhost = HeaderValue::from_static("http://localhost:3000");
    if !origins.contains(&localhost) {
        origins.push(localhost);
    }

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE]);

    let app = Router::new()
        .merge(api::router(state))
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "dashboard listening");

    axum::serve(listener, app).await?;
    Ok(())
}
