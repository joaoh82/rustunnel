//! rustunnel-server entry point.
//!
//! Wires all subsystems together:
//!   - SQLite database
//!   - TLS certificate manager (static PEM or ACME)
//!   - Control-plane WebSocket server
//!   - HTTP / HTTPS edge proxy
//!   - TCP edge proxy
//!   - Dashboard REST API + SPA
//!   - Prometheus metrics endpoint
//!   - Graceful shutdown on SIGINT / SIGTERM

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use axum::{routing::get, Router};
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use rustunnel_server::audit;
use rustunnel_server::config::ServerConfig;
use rustunnel_server::control::server::run_control_plane;
use rustunnel_server::core::{ControlMessage, TunnelCore};
use rustunnel_server::dashboard::run_dashboard;
use rustunnel_server::db;
use rustunnel_server::edge::{run_http_edge, run_tcp_edge, HttpEdgeConfig};
use rustunnel_server::error::Result;
use rustunnel_server::tls::CertManager;

// ── constants ─────────────────────────────────────────────────────────────────

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Capture channel capacity — drops events if the dashboard can't keep up.
const CAPTURE_CHANNEL_SIZE: usize = 1_024;

/// Fixed Prometheus metrics port.
const METRICS_PORT: u16 = 9090;

/// Maximum drain time during graceful shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "rustunnel-server",
    version = VERSION,
    about   = "Self-hosted secure tunnel server"
)]
struct Cli {
    /// Path to the server TOML configuration file.
    #[arg(short, long)]
    config: std::path::PathBuf,
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // rustls 0.23 requires an explicit CryptoProvider to be installed once
    // before any TLS operations.  Install ring as the process-wide default.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring provider");

    let cli = Cli::parse();

    // Load config before tracing is initialised so we can eprintln! on error.
    let config = match ServerConfig::from_file(&cli.config) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!(
                "fatal: failed to load config '{}': {e}",
                cli.config.display()
            );
            std::process::exit(1);
        }
    };

    init_tracing(&config);

    if let Err(e) = run(config).await {
        error!("server exited with error: {e}");
        std::process::exit(1);
    }
}

// ── main async body ───────────────────────────────────────────────────────────

async fn run(config: Arc<ServerConfig>) -> Result<()> {
    // ── database ──────────────────────────────────────────────────────────────

    info!(path = %config.database.path, "opening database");
    let pool = db::init_pool(&config.database.path).await?;
    info!("database ready");

    // ── audit logger ──────────────────────────────────────────────────────────

    let audit_tx = if let Some(path) = &config.logging.audit_log_path {
        info!(path, "audit logging enabled");
        audit::start_audit_logger(std::path::Path::new(path))
    } else {
        audit::noop_audit()
    };

    // ── tunnel core ───────────────────────────────────────────────────────────

    let core = Arc::new(TunnelCore::new(
        config.limits.tcp_port_range,
        config.limits.max_tunnels_per_session,
        config.limits.max_connections_per_tunnel,
        config.limits.ip_rate_limit_rps,
    ));

    // ── TLS certificate manager ───────────────────────────────────────────────

    info!(
        acme = config.tls.acme_enabled,
        cert = %config.tls.cert_path,
        "initialising TLS certificate manager"
    );
    let cert_manager = CertManager::new(Arc::clone(&config)).await?;
    let tls_handle = cert_manager.tls_handle(); // hot-swappable handle
    let tls_snapshot = cert_manager.get_tls_config(); // static snapshot for HTTP edge

    // ── capture channel ───────────────────────────────────────────────────────

    let (capture_tx, capture_rx) = mpsc::channel(CAPTURE_CHANNEL_SIZE);

    // ── socket addresses ──────────────────────────────────────────────────────

    let control_addr: SocketAddr = format!("0.0.0.0:{}", config.server.control_port)
        .parse()
        .unwrap();
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.server.http_port)
        .parse()
        .unwrap();
    let https_addr: SocketAddr = format!("0.0.0.0:{}", config.server.https_port)
        .parse()
        .unwrap();
    let dashboard_addr: SocketAddr = format!("0.0.0.0:{}", config.server.dashboard_port)
        .parse()
        .unwrap();
    let metrics_addr: SocketAddr = format!("0.0.0.0:{METRICS_PORT}").parse().unwrap();

    // ── startup banner ────────────────────────────────────────────────────────

    print_banner(&config);

    // ── task a: control-plane WebSocket server ────────────────────────────────

    let h_control = {
        let core = Arc::clone(&core);
        let cfg = Arc::clone(&config);
        let tls_handle = Arc::clone(&tls_handle);
        let audit_tx = audit_tx.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_control_plane(control_addr, core, cfg, tls_handle, audit_tx, pool).await
            {
                error!("control plane exited: {e}");
            }
        })
    };

    // ── task b: HTTP + HTTPS edge proxy ───────────────────────────────────────

    let h_http = {
        let core = Arc::clone(&core);
        let domain = config.server.domain.clone();
        let capture_tx = Some(capture_tx.clone());
        let limits = HttpEdgeConfig {
            rate_limit_rps: config.limits.rate_limit_rps,
            request_body_max_bytes: config.limits.request_body_max_bytes,
        };
        tokio::spawn(async move {
            if let Err(e) = run_http_edge(
                http_addr,
                https_addr,
                tls_snapshot,
                core,
                domain,
                capture_tx,
                limits,
            )
            .await
            {
                error!("HTTP edge exited: {e}");
            }
        })
    };

    // ── task c: TCP edge proxy ────────────────────────────────────────────────

    let h_tcp = {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            run_tcp_edge(core).await;
        })
    };

    // ── task d: dashboard API server ──────────────────────────────────────────

    let h_dashboard = {
        let core = Arc::clone(&core);
        let pool = pool.clone();
        let admin_token = config.auth.admin_token.clone();
        let audit_tx = audit_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_dashboard(
                dashboard_addr,
                core,
                pool,
                capture_rx,
                admin_token,
                audit_tx,
            )
            .await
            {
                error!("dashboard exited: {e}");
            }
        })
    };

    // ── task e: Prometheus metrics exporter ───────────────────────────────────

    let h_metrics = {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            if let Err(e) = run_metrics(metrics_addr, core).await {
                error!("metrics server exited: {e}");
            }
        })
    };

    // ── task f: ACME certificate renewal background task ──────────────────────

    Arc::clone(&cert_manager).start_renewal_task();

    // ── wait for shutdown signal ──────────────────────────────────────────────

    wait_for_shutdown_signal().await;

    info!("shutdown signal received — draining active sessions");

    // Notify every active session so it can close cleanly.
    let session_count = core.sessions.len();
    let mut notified = 0usize;
    for entry in core.sessions.iter() {
        let _ = entry.value().control_tx.try_send(ControlMessage::Shutdown);
        notified += 1;
    }
    info!(session_count, notified, "shutdown messages sent");

    // Stash abort handles before the drain future consumes the JoinHandles.
    let abort_handles = [
        h_control.abort_handle(),
        h_http.abort_handle(),
        h_tcp.abort_handle(),
        h_dashboard.abort_handle(),
        h_metrics.abort_handle(),
    ];

    // Allow up to 30 s for tasks to complete, then abort stragglers.
    let drain_result = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        let _ = h_control.await;
        let _ = h_http.await;
        let _ = h_tcp.await;
        let _ = h_dashboard.await;
        let _ = h_metrics.await;
    })
    .await;

    match drain_result {
        Ok(_) => info!("all tasks finished cleanly"),
        Err(_) => {
            warn!("shutdown timeout exceeded — aborting remaining tasks");
            for handle in &abort_handles {
                handle.abort();
            }
        }
    }

    info!("rustunnel-server stopped");
    Ok(())
}

// ── Prometheus metrics endpoint ───────────────────────────────────────────────

async fn run_metrics(addr: SocketAddr, core: Arc<TunnelCore>) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(core);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "metrics endpoint listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(State(core): State<Arc<TunnelCore>>) -> Response {
    let sessions = core.sessions.len();
    let http_tunnels = core.http_routes.len();
    let tcp_tunnels = core.tcp_routes.len();

    let body = format!(
        "# HELP rustunnel_active_sessions Number of active client sessions\n\
         # TYPE rustunnel_active_sessions gauge\n\
         rustunnel_active_sessions {sessions}\n\
         # HELP rustunnel_active_tunnels_http Number of active HTTP tunnels\n\
         # TYPE rustunnel_active_tunnels_http gauge\n\
         rustunnel_active_tunnels_http {http_tunnels}\n\
         # HELP rustunnel_active_tunnels_tcp Number of active TCP tunnels\n\
         # TYPE rustunnel_active_tunnels_tcp gauge\n\
         rustunnel_active_tunnels_tcp {tcp_tunnels}\n"
    );

    Response::builder()
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// ── tracing initialisation ────────────────────────────────────────────────────

fn init_tracing(config: &ServerConfig) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    if config.logging.format == "json" {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().pretty())
            .init();
    }
}

// ── shutdown signal handler ───────────────────────────────────────────────────

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c   => { info!("received SIGINT"); }
        _ = terminate => { info!("received SIGTERM"); }
    }
}

// ── startup banner ────────────────────────────────────────────────────────────

fn print_banner(config: &ServerConfig) {
    let domain = &config.server.domain;
    let ctrl_port = config.server.control_port;
    let http_port = config.server.http_port;
    let https_port = config.server.https_port;
    let dash_port = config.server.dashboard_port;
    let dash_url = format!("http://localhost:{dash_port}");

    println!();
    println!("  ██████╗ ██╗   ██╗███████╗████████╗██╗   ██╗███╗   ██╗███╗   ██╗███████╗██╗     ");
    println!("  ██╔══██╗██║   ██║██╔════╝╚══██╔══╝██║   ██║████╗  ██║████╗  ██║██╔════╝██║     ");
    println!("  ██████╔╝██║   ██║███████╗   ██║   ██║   ██║██╔██╗ ██║██╔██╗ ██║█████╗  ██║     ");
    println!("  ██╔══██╗██║   ██║╚════██║   ██║   ██║   ██║██║╚██╗██║██║╚██╗██║██╔══╝  ██║     ");
    println!("  ██║  ██║╚██████╔╝███████║   ██║   ╚██████╔╝██║ ╚████║██║ ╚████║███████╗███████╗");
    println!("  ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝    ╚═════╝ ╚═╝  ╚═══╝╚═╝  ╚═══╝╚══════╝╚══════╝");
    println!();
    println!("  ┌─────────────────────────────────────────────────┐");
    println!("  │  rustunnel-server v{VERSION:<30}│");
    println!("  ├─────────────────────────────────────────────────┤");
    println!("  │  Domain    {domain:<39}│");
    println!("  │  Control   :{ctrl_port:<38}│");
    println!("  │  HTTP      :{http_port:<38}│");
    println!("  │  HTTPS     :{https_port:<38}│");
    println!("  │  Dashboard :{dash_port:<38}│");
    println!("  │  Metrics   :{METRICS_PORT:<38}│");
    println!("  │  Sessions  {:<39}│", "0 (startup)");
    println!("  ├─────────────────────────────────────────────────┤");
    println!("  │  Dashboard → {dash_url:<36}│");
    println!("  └─────────────────────────────────────────────────┘");
    println!();
}
