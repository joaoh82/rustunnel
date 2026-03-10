//! Terminal display helpers.
//!
//! * `print_startup_box` — box with tunnel URLs shown once at startup.
//! * `print_request`     — one line per proxied HTTP request.

use chrono::Local;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Registered tunnel info shown in the startup box.
pub struct TunnelDisplay {
    pub name: String,
    pub proto: String,
    pub local: String,
    pub public_url: String,
}

/// Print the startup box with all active tunnel URLs.
pub fn print_startup_box(tunnels: &[TunnelDisplay]) {
    let width = 60usize;
    let border = style("─".repeat(width)).cyan().to_string();

    println!("{}", style("╭").cyan().to_string() + &border + &style("╮").cyan().to_string());
    println!(
        "{}  {}  {}",
        style("│").cyan(),
        style(format!("{:^56}", "rustunnel")).bold(),
        style("│").cyan()
    );
    println!("{}", style("├").cyan().to_string() + &border + &style("┤").cyan().to_string());

    for t in tunnels {
        let label = style(format!(" {:>4}", t.proto.to_uppercase())).bold().yellow();
        let name  = style(format!("[{}]", t.name)).dim();
        let arrow = style("→").dim();
        let local = style(&t.local).dim();
        let url   = style(&t.public_url).green().bold();
        println!("{} {} {} {} {}  {}", style("│").cyan(), label, name, arrow, local, style("│").cyan());
        println!("{}   {}  {}", style("│").cyan(), url, style("│").cyan());
    }

    println!("{}", style("╰").cyan().to_string() + &border + &style("╯").cyan().to_string());
    println!();
    println!(
        "  {} {}",
        style("✓").green().bold(),
        style("Tunnels active. Press Ctrl-C to quit.").dim()
    );
    println!();
}

/// Print a single proxied request line (timestamp | method | path | status | duration).
#[allow(dead_code)]
pub fn print_request(method: &str, path: &str, status: u16, duration_ms: u64) {
    let ts = Local::now().format("%H:%M:%S").to_string();
    let ts_str = style(ts).dim();

    let method_style = match method {
        "GET" => style(format!("{:>6}", method)).cyan(),
        "POST" => style(format!("{:>6}", method)).yellow(),
        "PUT" | "PATCH" => style(format!("{:>6}", method)).magenta(),
        "DELETE" => style(format!("{:>6}", method)).red(),
        _ => style(format!("{:>6}", method)).white(),
    };

    let status_style = match status {
        200..=299 => style(status.to_string()).green().bold(),
        300..=399 => style(status.to_string()).cyan(),
        400..=499 => style(status.to_string()).yellow().bold(),
        500..=599 => style(status.to_string()).red().bold(),
        _ => style(status.to_string()).white(),
    };

    let dur = if duration_ms < 100 {
        style(format!("{duration_ms}ms")).green()
    } else if duration_ms < 1000 {
        style(format!("{duration_ms}ms")).yellow()
    } else {
        style(format!("{}s", duration_ms / 1000)).red()
    };

    // Truncate path for display
    let display_path = if path.len() > 40 {
        format!("{}…", &path[..39])
    } else {
        path.to_string()
    };

    println!(
        "{} {} {:<42} {} {}",
        ts_str, method_style, display_path, status_style, dur
    );
}

/// Create a spinner for an in-progress operation (e.g. connecting).
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb.set_message(msg.to_string());
    pb
}
