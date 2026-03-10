//! Exponential-backoff reconnect loop.
//!
//! Wraps the `control::connect` function so that transient failures (network
//! drops, server restarts) result in automatic reconnection rather than
//! process exit.
//!
//! Delay schedule:
//!   initial = 1 s, multiplier = 2×, max = 60 s, jitter = ±20 %

use std::time::Duration;

use rand::Rng;
use tracing::{info, warn};

use crate::config::{ClientConfig, TunnelDef};
use crate::control;

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY:     Duration = Duration::from_secs(60);
const MULTIPLIER:    f64      = 2.0;
const JITTER:        f64      = 0.20; // ±20 %

/// Run `connect` with exponential-backoff retry on failure.
///
/// Returns only when the connection ends cleanly (e.g. Ctrl-C) or after a
/// fatal, non-retryable error.
pub async fn run_with_reconnect(config: ClientConfig, tunnels: Vec<TunnelDef>) {
    let mut delay = INITIAL_DELAY;
    let mut attempt: u32 = 0;

    loop {
        if attempt > 0 {
            eprintln!(
                "  Reconnecting in {:.1}s (attempt {attempt})…",
                delay.as_secs_f64()
            );
            tokio::time::sleep(delay).await;
            delay = next_delay(delay);
        }

        info!(attempt, "connecting to tunnel server");

        match control::connect(&config, &tunnels).await {
            Ok(()) => {
                // Clean exit (e.g. Ctrl-C) — stop retrying.
                info!("connection closed cleanly");
                return;
            }
            Err(e) => {
                // Auth failures are fatal — no point retrying.
                let err_str = e.to_string();
                if err_str.contains("auth") || err_str.contains("Auth") {
                    eprintln!("  Fatal: {e}");
                    return;
                }
                warn!("connection error: {e}");
                attempt += 1;
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn next_delay(current: Duration) -> Duration {
    let mut rng = rand::thread_rng();
    let jitter_factor = 1.0 + rng.gen_range(-JITTER..=JITTER);
    let next_secs = (current.as_secs_f64() * MULTIPLIER * jitter_factor)
        .min(MAX_DELAY.as_secs_f64());
    Duration::from_secs_f64(next_secs)
}
