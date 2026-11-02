//! rustunnel — self-hosted tunnel client
//!
//! Usage:
//!   rustunnel http <port> [options]
//!   rustunnel tcp  <port> [options]
//!   rustunnel start [--config <path>]
//!   rustunnel token create --name <name>

mod config;
mod control;
mod display;
mod error;
mod health;
mod inspect;
mod output;
mod p2p_direct;
mod proxy;
mod reconnect;
mod regions;
mod stun;
mod tui;
mod version;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use console::Term;
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;
use tracing_subscriber::EnvFilter;

use config::{ClientConfig, TunnelDef};
use inspect::{Exchange, Inspector, SessionStatus};

/// How long to wait for the tunnel session to wind down after the UI exits
/// before abandoning it. The process is on its way out either way.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "rustunnel",
    version,
    about   = "Expose local services through a secure tunnel",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an HTTP tunnel for a local port
    Http(TunnelArgs),

    /// Start a TCP tunnel for a local port
    Tcp(TunnelArgs),

    /// Start a UDP tunnel for a local port
    Udp(TunnelArgs),

    /// Start a P2P tunnel (publish a service or connect to a peer)
    P2p(P2pArgs),

    /// Start one or more tunnels defined in a config file
    Start(StartArgs),

    /// Manage API tokens
    Token(TokenCmd),

    /// Create a config file interactively (~/.rustunnel/config.yml)
    Setup,
}

#[derive(Args, Clone)]
struct TunnelArgs {
    /// Local port to forward
    port: u16,

    /// Request a specific subdomain (HTTP tunnels only)
    #[arg(long)]
    subdomain: Option<String>,

    /// Tunnel server address, e.g. tunnel.example.com:9000
    #[arg(long)]
    server: Option<String>,

    /// Auth token (overrides config file)
    #[arg(long, env = "RUSTUNNEL_TOKEN")]
    token: Option<String>,

    /// Local hostname to forward to
    #[arg(long, default_value = "localhost")]
    local_host: String,

    /// Region to connect to: eu, us, ap, or auto (probe nearest).
    /// Ignored if --server is specified.
    #[arg(long)]
    region: Option<String>,

    /// Disable automatic reconnection on failure
    #[arg(long)]
    no_reconnect: bool,

    /// Skip TLS certificate verification (local dev only — do not use in production)
    #[arg(long)]
    insecure: bool,

    /// Emit machine-readable NDJSON events (one JSON object per line) on
    /// stdout instead of human-readable output. Events: tunnel_ready,
    /// reconnecting, reconnected, error.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    ui: UiArgs,
}

/// Flags controlling the two user interfaces. Shared by every tunnel command.
#[derive(Args, Clone, Copy)]
struct UiArgs {
    /// Disable the full-screen terminal UI and print one line per request
    /// instead. Implied when stdout is not a terminal or `--json` is used.
    #[arg(long)]
    no_tui: bool,

    /// Port for the local web inspector. Scans forward if taken; 0 picks any
    /// free port.
    #[arg(long, default_value_t = inspect::server::DEFAULT_PORT)]
    inspect_port: u16,

    /// Disable the local web inspector.
    #[arg(long)]
    no_inspect: bool,
}

#[derive(Args, Clone)]
struct P2pArgs {
    /// Local port to forward
    port: u16,

    /// Shared secret for P2P authentication (both publisher and subscriber must match)
    #[arg(long)]
    secret: String,

    /// Publish a service under this name (publisher mode)
    #[arg(long, conflicts_with = "target")]
    name: Option<String>,

    /// Connect to a published P2P tunnel by name (subscriber mode)
    #[arg(long, conflicts_with = "name")]
    target: Option<String>,

    /// Tunnel server address
    #[arg(long)]
    server: Option<String>,

    /// Auth token (overrides config file)
    #[arg(long, env = "RUSTUNNEL_TOKEN")]
    token: Option<String>,

    /// Local hostname to forward to
    #[arg(long, default_value = "localhost")]
    local_host: String,

    /// Region to connect to
    #[arg(long)]
    region: Option<String>,

    /// Disable automatic reconnection on failure
    #[arg(long)]
    no_reconnect: bool,

    /// Skip TLS certificate verification (local dev only)
    #[arg(long)]
    insecure: bool,

    /// Emit machine-readable NDJSON events (one JSON object per line) on
    /// stdout instead of human-readable output. Events: tunnel_ready,
    /// reconnecting, reconnected, error.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    ui: UiArgs,
}

#[derive(Args)]
struct StartArgs {
    /// Path to config file (default: ~/.rustunnel/config.yml)
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// Emit machine-readable NDJSON events (one JSON object per line) on
    /// stdout instead of human-readable output. Events: tunnel_ready,
    /// reconnecting, reconnected, error.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    ui: UiArgs,
}

#[derive(Args)]
struct TokenCmd {
    #[command(subcommand)]
    action: TokenAction,
}

#[derive(Subcommand)]
enum TokenAction {
    /// Create a new API token via the dashboard REST API
    Create {
        /// Token label / name
        #[arg(long)]
        name: String,

        /// Dashboard server address, e.g. tunnel.example.com:4040
        #[arg(long)]
        server: Option<String>,

        /// Admin token for authentication
        #[arg(long)]
        admin_token: Option<String>,

        /// Emit the created token as a single machine-readable JSON line
        /// ({"event":"token_created",...}) instead of human-readable output
        #[arg(long)]
        json: bool,
    },
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring provider");

    init_tracing();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        if output::json_mode() {
            // Machine-readable fatal error: one JSON line on stdout, exit 1.
            output::emit(&output::Event::Error {
                code: e.code().to_string(),
                message: e.public_message(),
                hint: e.hint().map(str::to_string),
            });
        } else {
            eprintln!("{}", e.rendered());
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> error::Result<()> {
    match cli.command {
        Commands::Http(args) => run_tunnel("http", args).await,
        Commands::Tcp(args) => run_tunnel("tcp", args).await,
        Commands::Udp(args) => run_tunnel("udp", args).await,
        Commands::P2p(args) => run_p2p(args).await,
        Commands::Start(args) => run_start(args).await,
        Commands::Token(cmd) => run_token(cmd).await,
        Commands::Setup => run_setup().await,
    }
}

// ── subcommand handlers ───────────────────────────────────────────────────────

async fn run_tunnel(proto: &str, args: TunnelArgs) -> error::Result<()> {
    output::set_json_mode(args.json);

    let mut cfg = ClientConfig::load_default()?;

    // Token and insecure apply unconditionally. An empty/whitespace-only
    // value (e.g. `RUSTUNNEL_TOKEN=""` filling the arg via clap's env
    // support) counts as absent so it never clobbers a config-file token.
    if let Some(t) = args.token.filter(|t| !t.trim().is_empty()) {
        cfg.auth_token = Some(t);
        cfg.auth_token_source = Some("--token flag / RUSTUNNEL_TOKEN env var".into());
    }
    if args.insecure {
        cfg.insecure = true;
    }

    // Server resolution: explicit --server wins; otherwise use region logic.
    if let Some(explicit) = args.server {
        cfg.server = explicit;
    } else {
        cfg.server = regions::resolve_server(
            &cfg.server,
            args.region.as_deref(),
            cfg.region.as_deref(),
            cfg.insecure,
        )
        .await;
    }

    cfg.validate()?;

    let tunnels = vec![TunnelDef::from_cli(
        proto,
        args.port,
        &args.local_host,
        args.subdomain,
    )];

    run_session(cfg, tunnels, args.no_reconnect, args.ui).await
}

// ── session runner ────────────────────────────────────────────────────────────

/// Start the inspector and the terminal UI (when appropriate), then run the
/// tunnel session under them.
///
/// The terminal UI is skipped whenever stdout is not a terminal, `--no-tui` is
/// passed, or `--json` is active — those runs keep the original line-based
/// behaviour exactly.
async fn run_session(
    cfg: ClientConfig,
    tunnels: Vec<TunnelDef>,
    no_reconnect: bool,
    ui: UiArgs,
) -> error::Result<()> {
    let use_tui = !output::json_mode() && !ui.no_tui && std::io::stdout().is_terminal();
    let use_inspector = !ui.no_inspect;

    // HTTP is only parsed when something will actually display it.
    let inspector = Inspector::new(
        use_tui || use_inspector,
        cfg.server.clone(),
        cfg.region.clone(),
    );
    tui::set_log_sink(Arc::clone(&inspector));

    if use_inspector {
        match inspect::server::bind(ui.inspect_port).await {
            Some((listener, url)) => {
                inspector.set_inspect_url(url.clone());
                // Human modes print this under the startup box / in the TUI
                // header; JSON consumers get it as an event so the bound port
                // is discoverable rather than invisible.
                output::emit(&output::Event::InspectorReady { url });
                tokio::spawn(inspect::server::serve(listener, Arc::clone(&inspector)));
            }
            None => warn!(
                port = ui.inspect_port,
                "no free port for the local inspector — continuing without it"
            ),
        }
    }

    // Without the terminal UI, requests still stream to stdout one line each.
    if !use_tui && !output::json_mode() && inspector.capture_enabled() {
        tokio::spawn(print_request_lines(inspector.subscribe()));
    }

    if !use_tui {
        return run_tunnels(cfg, tunnels, no_reconnect, inspector).await;
    }

    // With the terminal UI, the session runs behind it. Whichever ends first
    // stops the other: quitting the UI shuts the session down, and a session
    // that dies wakes the UI so the terminal is restored before we report why.
    let mut session = tokio::spawn({
        let inspector = Arc::clone(&inspector);
        async move {
            let result = run_tunnels(cfg, tunnels, no_reconnect, Arc::clone(&inspector)).await;
            inspector.set_status(SessionStatus::Closed);
            inspector.request_shutdown();
            result
        }
    });

    let ui_result = tui::run(Arc::clone(&inspector)).await;
    inspector.request_shutdown();

    let session_result = match tokio::time::timeout(SHUTDOWN_GRACE, &mut session).await {
        Ok(Ok(result)) => result,
        // The task panicked or was cancelled; the UI already exited cleanly.
        Ok(Err(_)) => Ok(()),
        Err(_) => {
            session.abort();
            Ok(())
        }
    };

    ui_result.map_err(error::Error::Io)?;
    session_result
}

async fn run_tunnels(
    cfg: ClientConfig,
    tunnels: Vec<TunnelDef>,
    no_reconnect: bool,
    inspector: Arc<Inspector>,
) -> error::Result<()> {
    if no_reconnect {
        control::connect(&cfg, &tunnels, &inspector).await
    } else {
        reconnect::run_with_reconnect(cfg, tunnels, inspector).await
    }
}

/// Line-mode request log: one line per captured request, mirroring the terminal
/// UI's request table for pipes, CI, and `--no-tui`.
async fn print_request_lines(mut exchanges: tokio::sync::broadcast::Receiver<Arc<Exchange>>) {
    loop {
        match exchanges.recv().await {
            Ok(exchange) => display::print_request(
                &exchange.method,
                &exchange.path,
                exchange.status,
                exchange.duration_ms,
            ),
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        }
    }
}

async fn run_p2p(args: P2pArgs) -> error::Result<()> {
    output::set_json_mode(args.json);

    let mut cfg = ClientConfig::load_default()?;

    // Empty/whitespace-only token (e.g. `RUSTUNNEL_TOKEN=""`) counts as
    // absent so it never clobbers a config-file token.
    if let Some(t) = args.token.filter(|t| !t.trim().is_empty()) {
        cfg.auth_token = Some(t);
        cfg.auth_token_source = Some("--token flag / RUSTUNNEL_TOKEN env var".into());
    }
    if args.insecure {
        cfg.insecure = true;
    }
    if let Some(explicit) = args.server {
        cfg.server = explicit;
    } else {
        cfg.server = regions::resolve_server(
            &cfg.server,
            args.region.as_deref(),
            cfg.region.as_deref(),
            cfg.insecure,
        )
        .await;
    }

    cfg.validate()?;

    // Determine publisher vs subscriber mode.
    let tunnel = if let Some(name) = args.name {
        // Publisher mode: expose a local service under a P2P name.
        TunnelDef::p2p_publisher(args.port, &args.local_host, name, args.secret)
    } else if let Some(target) = args.target {
        // Subscriber mode: connect to a remote P2P tunnel.
        TunnelDef::p2p_subscriber(args.port, &args.local_host, target, args.secret)
    } else {
        return Err(error::Error::Config(
            "P2P mode requires either --name (publisher) or --target (subscriber)".into(),
        ));
    };

    let tunnels = vec![tunnel];

    run_session(cfg, tunnels, args.no_reconnect, args.ui).await
}

async fn run_start(args: StartArgs) -> error::Result<()> {
    output::set_json_mode(args.json);

    let mut cfg = match args.config {
        Some(path) => ClientConfig::load_from(&path)?,
        None => ClientConfig::load_default()?,
    };

    // Apply region from config (no CLI --region flag for `start`).
    if cfg.region.is_some() {
        cfg.server =
            regions::resolve_server(&cfg.server, None, cfg.region.as_deref(), cfg.insecure).await;
    }

    cfg.validate()?;

    if cfg.tunnels.is_empty() {
        return Err(error::Error::Config(
            "no tunnels defined in config file".into(),
        ));
    }

    let tunnels: Vec<TunnelDef> = cfg.tunnels.values().cloned().collect();
    run_session(cfg, tunnels, false, args.ui).await
}

async fn run_token(cmd: TokenCmd) -> error::Result<()> {
    match cmd.action {
        TokenAction::Create {
            name,
            server,
            admin_token,
            json,
        } => {
            output::set_json_mode(json);

            let dashboard = server.unwrap_or_else(|| "localhost:4040".to_string());
            let token = admin_token.unwrap_or_default();

            let url = format!("http://{dashboard}/api/tokens");
            let client = reqwest::Client::new();
            let resp = client
                .post(&url)
                .bearer_auth(&token)
                .json(&serde_json::json!({ "label": name }))
                .send()
                .await
                .map_err(|e| {
                    error::Error::Connection(format!(
                        "cannot reach dashboard API at {url} ({e}) — \
                         pass --server <host:port> of the dashboard API"
                    ))
                })?;

            if resp.status().is_success() {
                let body: serde_json::Value = resp.json().await.map_err(|e| {
                    error::Error::Connection(format!("invalid response from {url}: {e}"))
                })?;
                if output::json_mode() {
                    output::emit(&output::Event::TokenCreated {
                        token: body["token"].as_str().unwrap_or("?").to_string(),
                        name: body["label"].as_str().unwrap_or(&name).to_string(),
                        id: body["id"].as_str().map(str::to_string),
                    });
                } else {
                    println!("Token created:");
                    println!("  id:    {}", body["id"].as_str().unwrap_or("?"));
                    println!("  token: {}", body["token"].as_str().unwrap_or("?"));
                    println!("  label: {}", body["label"].as_str().unwrap_or("?"));
                }
            } else {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.as_u16() == 401 || status.as_u16() == 403 {
                    return Err(error::Error::Auth(format!(
                        "token creation rejected by {dashboard} ({status}): {text} — \
                         pass a valid --admin-token (the server's admin token)"
                    )));
                }
                return Err(error::Error::Connection(format!(
                    "token creation failed ({status} from {dashboard}): {text}"
                )));
            }
        }
    }
    Ok(())
}

async fn run_setup() -> error::Result<()> {
    let term = Term::stdout();

    term.write_line("rustunnel setup — create ~/.rustunnel/config.yml")?;
    term.write_line("")?;

    // 1. Region prompt (first — determines server)
    term.write_line("Region [auto / eu / us / ap / self-hosted] (default: auto): ")?;
    let region_input = term.read_line()?;
    let region_choice = region_input.trim().to_lowercase();

    let (server, region) = match region_choice.as_str() {
        // Known managed region: resolve server automatically
        "eu" | "us" | "ap" => {
            let srv = regions::server_for_region(&region_choice)
                .expect("built-in region lookup must succeed for eu/us/ap");
            term.write_line(&format!("  Server set to: {srv}"))?;
            (srv, Some(region_choice))
        }

        // Auto: probe all regions, pick nearest
        "" | "auto" => {
            let srv = regions::auto_select_nearest().await;
            term.write_line(&format!("  Server set to: {srv}"))?;
            (srv, Some("auto".to_string()))
        }

        // Self-hosted: user provides their own server
        "self-hosted" => {
            term.write_line("")?;
            term.write_line("Tunnel server address: ")?;
            let server_input = term.read_line()?;
            let srv = server_input.trim().to_string();
            if srv.is_empty() {
                return Err(error::Error::Config(
                    "server address is required for self-hosted mode".into(),
                ));
            }
            (srv, None) // no region stored
        }

        other => {
            return Err(error::Error::Config(format!(
                "unknown region '{other}' — choose auto, eu, us, ap, or self-hosted"
            )));
        }
    };

    // 2. Auth token prompt
    term.write_line("")?;
    term.write_line("Auth token (leave blank to skip): ")?;
    let token_input = term.read_line()?;
    let auth_token = token_input.trim().to_string();

    // Build config file contents
    let auth_token_line = if auth_token.is_empty() {
        "# auth_token: your-token-here".to_string()
    } else {
        format!("auth_token: {auth_token}")
    };

    let region_line = match &region {
        Some(r) => format!("region: {r}"),
        None => "# region: not applicable (self-hosted)".to_string(),
    };

    let contents = format!(
        r#"# rustunnel configuration
# Documentation: https://github.com/joaoh82/rustunnel

server: {server}
{auth_token_line}
{region_line}

# tunnels:
#   web:
#     proto: http
#     local_port: 3000
#   api:
#     proto: http
#     local_port: 8080
#     subdomain: myapi
#   database:
#     proto: tcp
#     local_port: 5432
"#
    );

    // Write config file
    let home = dirs::home_dir()
        .ok_or_else(|| error::Error::Config("cannot determine home directory".into()))?;
    let config_dir = home.join(".rustunnel");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        error::Error::Config(format!("cannot create {}: {e}", config_dir.display()))
    })?;

    let config_path = config_dir.join("config.yml");
    let exists = config_path.exists();
    std::fs::write(&config_path, &contents).map_err(|e| {
        error::Error::Config(format!("cannot write {}: {e}", config_path.display()))
    })?;

    term.write_line("")?;
    if exists {
        term.write_line(&format!("Updated: {}", config_path.display()))?;
    } else {
        term.write_line(&format!("Created: {}", config_path.display()))?;
    }
    term.write_line("Run `rustunnel start` to connect using this config.")?;

    Ok(())
}

// ── tracing init ──────────────────────────────────────────────────────────────

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        // Diagnostics go to stderr so stdout stays clean for tunnel output
        // (and valid NDJSON in --json mode). While the terminal UI owns the
        // screen they are buffered for its log pane instead of corrupting it.
        .with_writer(tui::LogWriter)
        .with_ansi(std::io::stderr().is_terminal())
        .compact()
        .init();
}
