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
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use rustunnel_server::audit;
use rustunnel_server::config::ServerConfig;
use rustunnel_server::control::server::run_control_plane;
use rustunnel_server::core::{ControlMessage, TunnelCore};
use rustunnel_server::dashboard::run_dashboard;
use rustunnel_server::db;
use rustunnel_server::edge::{run_http_edge, run_tcp_edge, run_udp_edge, HttpEdgeConfig};
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
    #[arg(short, long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Recompute usage_daily for a UTC date range, then exit.
    /// Does not start the server and is never run on startup.
    Backfill {
        /// First UTC day, inclusive (YYYY-MM-DD).
        #[arg(long)]
        from: chrono::NaiveDate,
        /// Last UTC day, inclusive (YYYY-MM-DD). Defaults to today UTC.
        #[arg(long)]
        to: Option<chrono::NaiveDate>,
    },
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
    let Some(config_path) = cli.config else {
        eprintln!("fatal: --config is required");
        std::process::exit(1);
    };

    // Load config before tracing is initialised so we can eprintln! on error.
    let config = match ServerConfig::from_file(&config_path) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!(
                "fatal: failed to load config '{}': {e}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    // Initialise Sentry before tracing so panics are captured immediately.
    // The guard must outlive the entire process — dropping it flushes events.
    let _sentry_guard = init_sentry(&config);

    init_tracing(&config);

    if let Some(CliCommand::Backfill { from, to }) = cli.command {
        if let Err(e) = backfill_usage(config, from, to).await {
            error!("usage backfill failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run(config).await {
        error!("server exited with error: {e}");
        std::process::exit(1);
    }
}

/// One-shot historical recompute. Not called from [`run`], so a normal
/// start or deploy never backfills. Retries a few times if a live edge
/// holds the rollup lock for a single tick.
async fn backfill_usage(
    config: Arc<ServerConfig>,
    from: chrono::NaiveDate,
    to: Option<chrono::NaiveDate>,
) -> Result<()> {
    let to = to.unwrap_or_else(|| chrono::Utc::now().date_naive());
    if from > to {
        return Err(rustunnel_server::error::Error::Config(format!(
            "backfill --from {from} is after --to {to}"
        )));
    }
    info!(%from, %to, "usage backfill starting");
    let db = db::init_db(&config.database).await?;
    for attempt in 1..=5 {
        match db::rollup_usage_between(&db.pg, from, to, chrono::Utc::now()).await? {
            db::RollupOutcome::Applied(applied) => {
                info!(
                    days = applied.days,
                    rows = applied.rows,
                    "usage backfill complete"
                );
                return Ok(());
            }
            db::RollupOutcome::SkippedBusy => {
                warn!(attempt, "usage rollup lock is busy, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(rustunnel_server::error::Error::Config(
        "usage backfill skipped: another rollup holds the advisory lock".into(),
    ))
}

// ── main async body ───────────────────────────────────────────────────────────

async fn run(config: Arc<ServerConfig>) -> Result<()> {
    // ── database ──────────────────────────────────────────────────────────────

    info!(url = %config.database.url, "connecting to PostgreSQL");
    let db = db::init_db(&config.database).await?;
    let stale = db::close_stale_tunnels(&db.pg, &config.region.id).await?;
    if stale > 0 {
        info!(
            count = stale,
            region = %config.region.id,
            "closed stale tunnel_log rows from previous run in this region"
        );
    }
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
        config.limits.udp_port_range,
        config.limits.max_tunnels_per_session,
        config.limits.max_connections_per_tunnel,
        config.limits.ip_rate_limit_rps,
        config.auth.max_failed_auth_per_minute,
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
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_control_plane(control_addr, core, cfg, tls_handle, audit_tx, db).await
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
            plain_http_mode: config.server.plain_http_mode,
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

    // ── task c2: UDP edge proxy ──────────────────────────────────────────────

    let h_udp = {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            run_udp_edge(core).await;
        })
    };

    // ── task d: dashboard API server ──────────────────────────────────────────

    let h_dashboard = {
        let core = Arc::clone(&core);
        let admin_token = config.auth.admin_token.clone();
        let dashboard_origin = config.server.dashboard_origin.clone();
        let audit_tx = audit_tx.clone();
        let region = config.region.clone();
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = run_dashboard(
                dashboard_addr,
                core,
                db,
                capture_rx,
                admin_token,
                audit_tx,
                dashboard_origin,
                region,
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
        let region = config.region.id.clone();
        tokio::spawn(async move {
            if let Err(e) = run_metrics(metrics_addr, core, region).await {
                error!("metrics server exited: {e}");
            }
        })
    };

    // ── task f: ACME certificate renewal background task ──────────────────────

    Arc::clone(&cert_manager).start_renewal_task();

    // ── task g: periodic meter flush ──────────────────────────────────────────
    //
    // Writes absolute counters onto open tunnel_log rows so a crash loses at
    // most one interval. `0` disables the task. Shutdown flushes either way.

    let h_meter = {
        let core = Arc::clone(&core);
        let db = db.clone();
        let interval = Duration::from_secs(config.metering.flush_interval_secs);
        tokio::spawn(async move {
            if interval.is_zero() {
                info!("periodic tunnel meter flush disabled");
                return;
            }
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // the first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                match db::flush_live_meters(&db.pg, &core).await {
                    Ok(n) if n > 0 => debug!(count = n, "flushed live tunnel meters"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "periodic tunnel meter flush failed"),
                }
            }
        })
    };

    // ── task h: usage_daily rollup ────────────────────────────────────────────
    //
    // One edge wins the advisory lock and recomputes today and yesterday.
    // The others skip. `interval_secs = 0` disables the timer. Historical
    // days are only filled by the `backfill` subcommand.

    let h_rollup = {
        let db = db.clone();
        let interval_secs = config.usage_rollup.interval_secs;
        let lookback_days = config.usage_rollup.lookback_days;
        tokio::spawn(async move {
            if interval_secs == 0 {
                info!("usage rollup timer disabled");
                return;
            }
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                match db::rollup_recent(&db.pg, lookback_days, chrono::Utc::now()).await {
                    Ok(db::RollupOutcome::Applied(applied)) => {
                        info!(
                            days = applied.days,
                            rows = applied.rows,
                            "usage rollup applied"
                        );
                    }
                    Ok(db::RollupOutcome::SkippedBusy) => {
                        debug!("usage rollup skipped; another edge holds the lock");
                    }
                    Err(e) => warn!(error = %e, "usage rollup failed"),
                }
            }
        })
    };

    // ── wait for shutdown signal ──────────────────────────────────────────────

    wait_for_shutdown_signal().await;

    info!("shutdown signal received, flushing tunnel meters before close");
    if let Err(e) = db::flush_live_meters(&db.pg, &core).await {
        warn!(error = %e, "shutdown meter flush failed");
    }

    info!("draining active sessions");

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
        h_udp.abort_handle(),
        h_dashboard.abort_handle(),
        h_metrics.abort_handle(),
        h_meter.abort_handle(),
        h_rollup.abort_handle(),
    ];

    // Allow up to 30 s for tasks to complete, then abort stragglers.
    let drain_result = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        let _ = h_control.await;
        let _ = h_http.await;
        let _ = h_tcp.await;
        let _ = h_udp.await;
        let _ = h_dashboard.await;
        let _ = h_metrics.await;
        let _ = h_meter.await;
        let _ = h_rollup.await;
    })
    .await;

    // Sessions that exited wrote their own close. Anything still registered
    // is flushed again and closed here, so bytes gathered during the drain
    // are not left only in memory.
    if let Err(e) = db::flush_and_close_live_meters(&db.pg, &core).await {
        warn!(error = %e, "final shutdown meter close failed");
    }

    match drain_result {
        Ok(_) => info!("all tasks finished cleanly"),
        Err(_) => {
            warn!("shutdown timeout exceeded, aborting remaining tasks");
            for handle in &abort_handles {
                handle.abort();
            }
        }
    }

    info!("rustunnel-server stopped");
    Ok(())
}

// ── Prometheus metrics endpoint ───────────────────────────────────────────────

/// State for the metrics endpoint: the routing tables plus the region id
/// (used as a Prometheus label so a central scraper can distinguish series
/// from EU / US / AP).
#[derive(Clone)]
struct MetricsState {
    core: Arc<TunnelCore>,
    region: String,
}

/// Escape a label value per the Prometheus exposition format:
/// backslashes, double-quotes, and newlines are the only forbidden
/// characters inside `"..."`. We don't expect any of these in real
/// region/group names but defensive-encoding keeps malformed config
/// from breaking the scrape.
fn escape_prometheus_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            other => out.push(other),
        }
    }
    out
}

async fn run_metrics(addr: SocketAddr, core: Arc<TunnelCore>, region: String) -> Result<()> {
    let state = MetricsState { core, region };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "metrics endpoint listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(State(state): State<MetricsState>) -> Response {
    use std::sync::atomic::Ordering;
    let core = &state.core;
    let region_label = escape_prometheus_label(&state.region);

    let sessions = core.sessions.len();
    // Existing gauges count *members* (i.e. registered tunnels) so the
    // numbers stay comparable with pre-load-balancing data. Phase 5 layers
    // separate `rustunnel_group_*` series on top, keyed by group identity.
    let http_tunnels: usize = core.http_routes.iter().map(|g| g.members.len()).sum();
    let tcp_tunnels: usize = core.tcp_routes.iter().map(|g| g.members.len()).sum();
    let udp_tunnels: usize = core.udp_routes.iter().map(|g| g.members.len()).sum();
    let p2p_tunnels = core.p2p_tunnels.len();

    // Compute aggregate bytes and request counts across all tunnel types.
    let mut total_bytes: u64 = 0;
    let mut total_requests: u64 = 0;
    for group in core.http_routes.iter() {
        for member in group.members.iter() {
            total_bytes += member.info.bytes_proxied.load(Ordering::Relaxed);
            total_requests += member.info.request_count.load(Ordering::Relaxed);
        }
    }
    for group in core.tcp_routes.iter() {
        for member in group.members.iter() {
            total_bytes += member.info.bytes_proxied.load(Ordering::Relaxed);
            total_requests += member.info.request_count.load(Ordering::Relaxed);
        }
    }
    for group in core.udp_routes.iter() {
        for member in group.members.iter() {
            total_bytes += member.info.bytes_proxied.load(Ordering::Relaxed);
            total_requests += member.info.request_count.load(Ordering::Relaxed);
        }
    }
    for entry in core.p2p_tunnels.iter() {
        total_bytes += entry
            .tunnel_info
            .bytes_proxied
            .load(std::sync::atomic::Ordering::Relaxed);
        total_requests += entry
            .tunnel_info
            .request_count
            .load(std::sync::atomic::Ordering::Relaxed);
    }

    // ── Phase 5: per-group series ────────────────────────────────────────
    //
    // For each grouped (key_hash.is_some()) routing entry across HTTP and
    // TCP, emit:
    //   - `rustunnel_group_members{group, region, healthy="true|false"}` —
    //     gauge of currently-registered members partitioned by health bit.
    //   - `rustunnel_group_dispatches_total{group, region}` — sum of
    //     per-member request counts. Per-group, *not* per-member, to keep
    //     the label cardinality bounded against UUID churn (see plan §7).
    //   - `rustunnel_group_health_failures_total{group, region, kind}` —
    //     sum of `total_health_failures` across members; `kind` is the
    //     probe type (tcp/http) drawn from the first member's spec.
    //
    // Solo (ungrouped) registrations are skipped — those still show up in
    // the existing `rustunnel_active_tunnels_*` gauges and
    // `rustunnel_requests_total` counter.
    let mut groups_block = String::new();
    groups_block.push_str(
        "# HELP rustunnel_group_members Number of registered group members partitioned by healthy state\n\
         # TYPE rustunnel_group_members gauge\n\
         # HELP rustunnel_group_dispatches_total Total dispatched connections across all members of a group\n\
         # TYPE rustunnel_group_dispatches_total counter\n\
         # HELP rustunnel_group_health_failures_total Total TunnelUnhealthy frames received across the group's members\n\
         # TYPE rustunnel_group_health_failures_total counter\n",
    );

    let mut emit_group = |routes_iter: Box<
        dyn Iterator<Item = (String, Arc<rustunnel_server::core::TunnelGroup>)> + '_,
    >| {
        for (_route_key, group) in routes_iter {
            if group.key_hash.is_none() {
                continue; // solo / ungrouped — covered by the legacy gauges
            }
            let group_label = escape_prometheus_label(&group.name);
            let mut healthy = 0u64;
            let mut unhealthy = 0u64;
            let mut dispatches = 0u64;
            let mut failures = 0u64;
            let mut probe_kind: Option<&'static str> = None;
            for member in group.members.iter() {
                if member.healthy.load(Ordering::Acquire) {
                    healthy += 1;
                } else {
                    unhealthy += 1;
                }
                dispatches += member.info.request_count.load(Ordering::Relaxed);
                failures += member.total_health_failures.load(Ordering::Relaxed);
                if probe_kind.is_none() {
                    probe_kind = member.health_spec.as_ref().map(|s| match s.kind {
                        rustunnel_protocol::HealthCheckKind::Tcp => "tcp",
                        rustunnel_protocol::HealthCheckKind::Http => "http",
                    });
                }
            }
            use std::fmt::Write;
            let _ = writeln!(
                groups_block,
                "rustunnel_group_members{{group=\"{group_label}\",region=\"{region_label}\",healthy=\"true\"}} {healthy}"
            );
            let _ = writeln!(
                groups_block,
                "rustunnel_group_members{{group=\"{group_label}\",region=\"{region_label}\",healthy=\"false\"}} {unhealthy}"
            );
            let _ = writeln!(
                groups_block,
                "rustunnel_group_dispatches_total{{group=\"{group_label}\",region=\"{region_label}\"}} {dispatches}"
            );
            // `kind` defaults to "none" when no member has opted into probes
            // — that's a meaningful case operators may want to filter on
            // (e.g. "groups with health checks vs without").
            let kind = probe_kind.unwrap_or("none");
            let _ = writeln!(
                groups_block,
                "rustunnel_group_health_failures_total{{group=\"{group_label}\",region=\"{region_label}\",kind=\"{kind}\"}} {failures}"
            );
        }
    };
    emit_group(Box::new(
        core.http_routes
            .iter()
            .map(|e| (e.key().clone(), Arc::clone(e.value()))),
    ));
    emit_group(Box::new(
        core.tcp_routes
            .iter()
            .map(|e| (e.key().to_string(), Arc::clone(e.value()))),
    ));

    let body = format!(
        "# HELP rustunnel_active_sessions Number of active client sessions\n\
         # TYPE rustunnel_active_sessions gauge\n\
         rustunnel_active_sessions {sessions}\n\
         # HELP rustunnel_active_tunnels_http Number of active HTTP tunnels\n\
         # TYPE rustunnel_active_tunnels_http gauge\n\
         rustunnel_active_tunnels_http {http_tunnels}\n\
         # HELP rustunnel_active_tunnels_tcp Number of active TCP tunnels\n\
         # TYPE rustunnel_active_tunnels_tcp gauge\n\
         rustunnel_active_tunnels_tcp {tcp_tunnels}\n\
         # HELP rustunnel_active_tunnels_udp Number of active UDP tunnels\n\
         # TYPE rustunnel_active_tunnels_udp gauge\n\
         rustunnel_active_tunnels_udp {udp_tunnels}\n\
         # HELP rustunnel_active_tunnels_p2p Number of active P2P tunnels\n\
         # TYPE rustunnel_active_tunnels_p2p gauge\n\
         rustunnel_active_tunnels_p2p {p2p_tunnels}\n\
         # HELP rustunnel_bytes_proxied_total Total bytes proxied across all tunnels\n\
         # TYPE rustunnel_bytes_proxied_total counter\n\
         rustunnel_bytes_proxied_total {total_bytes}\n\
         # HELP rustunnel_requests_total Total requests/connections across all tunnels\n\
         # TYPE rustunnel_requests_total counter\n\
         rustunnel_requests_total {total_requests}\n\
         {groups_block}"
    );

    Response::builder()
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// ── Sentry initialisation ────────────────────────────────────────────────────

fn init_sentry(config: &ServerConfig) -> sentry::ClientInitGuard {
    // DSN priority: SENTRY_DSN env var > config file > disabled.
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .or_else(|| config.logging.sentry_dsn.clone())
        .unwrap_or_default();

    sentry::init((
        dsn,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            environment: Some(
                std::env::var("SENTRY_ENVIRONMENT")
                    .unwrap_or_else(|_| "production".into())
                    .into(),
            ),
            send_default_pii: true,
            traces_sample_rate: 1.0,
            enable_logs: true,
            ..Default::default()
        },
    ))
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
            .with(sentry_tracing::layer())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().pretty())
            .with(sentry_tracing::layer())
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
    let region_id = &config.region.id;
    let region_loc = if config.region.location.is_empty() {
        "—".to_string()
    } else {
        config.region.location.clone()
    };

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
    println!("  │  Region    {region_id:<39}│");
    println!("  │  Location  {region_loc:<39}│");
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
