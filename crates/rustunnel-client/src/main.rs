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
mod proxy;
mod reconnect;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use console::Term;
use tracing_subscriber::EnvFilter;

use config::{ClientConfig, TunnelDef};

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
    #[arg(long)]
    token: Option<String>,

    /// Local hostname to forward to
    #[arg(long, default_value = "localhost")]
    local_host: String,

    /// Disable automatic reconnection on failure
    #[arg(long)]
    no_reconnect: bool,

    /// Skip TLS certificate verification (local dev only — do not use in production)
    #[arg(long)]
    insecure: bool,
}

#[derive(Args)]
struct StartArgs {
    /// Path to config file (default: ~/.rustunnel/config.yml)
    #[arg(long, short)]
    config: Option<PathBuf>,
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
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> error::Result<()> {
    match cli.command {
        Commands::Http(args) => run_tunnel("http", args).await,
        Commands::Tcp(args) => run_tunnel("tcp", args).await,
        Commands::Start(args) => run_start(args).await,
        Commands::Token(cmd) => run_token(cmd).await,
        Commands::Setup => run_setup().await,
    }
}

// ── subcommand handlers ───────────────────────────────────────────────────────

async fn run_tunnel(proto: &str, args: TunnelArgs) -> error::Result<()> {
    let mut cfg = ClientConfig::load_default()?;
    cfg.apply_overrides(args.server, args.token, args.insecure);
    cfg.validate()?;

    let tunnels = vec![TunnelDef::from_cli(
        proto,
        args.port,
        &args.local_host,
        args.subdomain,
    )];

    if args.no_reconnect {
        control::connect(&cfg, &tunnels).await
    } else {
        reconnect::run_with_reconnect(cfg, tunnels).await;
        Ok(())
    }
}

async fn run_start(args: StartArgs) -> error::Result<()> {
    let cfg = match args.config {
        Some(path) => ClientConfig::load_from(&path)?,
        None => ClientConfig::load_default()?,
    };
    cfg.validate()?;

    if cfg.tunnels.is_empty() {
        return Err(error::Error::Config(
            "no tunnels defined in config file".into(),
        ));
    }

    let tunnels: Vec<TunnelDef> = cfg.tunnels.values().cloned().collect();
    reconnect::run_with_reconnect(cfg, tunnels).await;
    Ok(())
}

async fn run_token(cmd: TokenCmd) -> error::Result<()> {
    match cmd.action {
        TokenAction::Create {
            name,
            server,
            admin_token,
        } => {
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
                .map_err(|e| error::Error::Connection(e.to_string()))?;

            if resp.status().is_success() {
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| error::Error::Connection(e.to_string()))?;
                println!("Token created:");
                println!("  id:    {}", body["id"].as_str().unwrap_or("?"));
                println!("  token: {}", body["token"].as_str().unwrap_or("?"));
                println!("  label: {}", body["label"].as_str().unwrap_or("?"));
            } else {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(error::Error::Connection(format!(
                    "token creation failed ({status}): {text}"
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

    // Server prompt
    term.write_line("Tunnel server address [edge.rustunnel.com:4040]: ")?;
    let server_input = term.read_line()?;
    let server = if server_input.trim().is_empty() {
        "edge.rustunnel.com:4040".to_string()
    } else {
        server_input.trim().to_string()
    };

    // Auth token prompt
    term.write_line("Auth token (leave blank to skip): ")?;
    let token_input = term.read_line()?;
    let auth_token = token_input.trim().to_string();

    // Build config file contents
    let auth_token_line = if auth_token.is_empty() {
        "# auth_token: your-token-here".to_string()
    } else {
        format!("auth_token: {auth_token}")
    };

    let contents = format!(
        r#"# rustunnel configuration
# Documentation: https://github.com/joaoh82/rustunnel

server: {server}
{auth_token_line}

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
        .compact()
        .init();
}
