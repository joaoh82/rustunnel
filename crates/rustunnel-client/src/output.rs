//! Output routing: human-readable (default) vs machine-readable NDJSON (`--json`).
//!
//! When `--json` is passed, stdout carries one JSON object per line (NDJSON)
//! instead of the human-readable startup box / progress text. Each line is a
//! self-contained event object with an `"event"` discriminator field:
//!
//! ```json
//! {"event":"inspector_ready","url":"http://127.0.0.1:4040"}
//! {"event":"tunnel_ready","protocol":"http","public_url":"https://x.example.com","local_port":3000,...}
//! {"event":"reconnecting","attempt":1,"reason":"connection error: ...","delay_secs":1.0}
//! {"event":"reconnected"}
//! {"event":"error","code":"connection","message":"...","hint":"..."}
//! {"event":"token_created","token":"...","name":"..."}
//! ```
//!
//! Human-readable output is unchanged when the flag is absent. Diagnostics
//! (tracing, spinner) always go to stderr, so stdout stays valid NDJSON.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

static JSON_MODE: AtomicBool = AtomicBool::new(false);

/// Set once at startup from the `--json` CLI flag.
pub fn set_json_mode(enabled: bool) {
    JSON_MODE.store(enabled, Ordering::Relaxed);
}

/// True when `--json` was passed: stdout must carry only NDJSON events.
pub fn json_mode() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

// Tracks an in-progress reconnect so that the next successful connection can
// emit a `reconnected` event before its `tunnel_ready` events.
static RECONNECT_PENDING: AtomicBool = AtomicBool::new(false);

/// Record that a reconnect attempt is in progress.
pub fn note_reconnecting() {
    RECONNECT_PENDING.store(true, Ordering::Relaxed);
}

/// Consume the pending-reconnect marker. Returns true exactly once after
/// `note_reconnecting` was called.
pub fn take_reconnect_pending() -> bool {
    RECONNECT_PENDING.swap(false, Ordering::Relaxed)
}

/// One NDJSON event. Serialized as a single JSON object with an `"event"`
/// discriminator (snake_case variant name).
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The local request inspector is listening. Emitted once at startup,
    /// before any `tunnel_ready`, and omitted entirely with `--no-inspect` —
    /// so the listening port is never a silent side effect in automation.
    InspectorReady { url: String },
    /// A tunnel is registered and ready to receive traffic.
    /// `public_url` is always set; `public_addr` (host:port) is additionally
    /// set for tcp/udp tunnels.
    TunnelReady {
        protocol: String,
        public_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        public_addr: Option<String>,
        local_port: u16,
        local_host: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tunnel_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// The connection dropped; the client will retry after `delay_secs`.
    Reconnecting {
        attempt: u32,
        reason: String,
        delay_secs: f64,
    },
    /// A reconnect attempt succeeded; fresh `tunnel_ready` events follow.
    Reconnected,
    /// Server-attached notice. Today this is the Hobby bandwidth warning
    /// printed when month-to-date usage is at least 80% of the plan cap.
    Notice { message: String },
    /// Fatal error — the process exits with code 1 after this line.
    Error {
        code: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// `rustunnel token create` succeeded.
    TokenCreated {
        token: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

impl Event {
    /// Build a `tunnel_ready` event, deriving `public_addr` from `public_url`
    /// for tcp/udp tunnels (`tcp://host:port` → `host:port`).
    pub fn tunnel_ready(
        protocol: &str,
        public_url: &str,
        local_port: u16,
        local_host: &str,
        tunnel_id: Option<String>,
        name: Option<String>,
    ) -> Self {
        let public_addr = public_url
            .strip_prefix("tcp://")
            .or_else(|| public_url.strip_prefix("udp://"))
            .map(str::to_string);
        Event::TunnelReady {
            protocol: protocol.to_string(),
            public_url: public_url.to_string(),
            public_addr,
            local_port,
            local_host: local_host.to_string(),
            tunnel_id,
            name,
        }
    }
}

/// Write one event as a single NDJSON line to stdout. No-op unless `--json`
/// mode is active, so call sites can emit unconditionally.
pub fn emit(event: &Event) {
    if !json_mode() {
        return;
    }
    // Event serialization is infallible: no maps with non-string keys, no
    // custom Serialize impls.
    let line = serde_json::to_string(event).expect("NDJSON event serialization cannot fail");
    // Rust ignores SIGPIPE, so when the NDJSON consumer closes the pipe early
    // (`rustunnel ... --json | head -1`) the write fails with BrokenPipe
    // instead of killing the process. `println!` would panic on that; treat it
    // as a normal end of output and exit cleanly.
    use std::io::Write;
    if let Err(e) = writeln!(std::io::stdout().lock(), "{line}") {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_ready_http_serializes_with_public_url_only() {
        let ev = Event::tunnel_ready(
            "http",
            "https://myapp.edge.rustunnel.com",
            3000,
            "localhost",
            Some("a1b2".into()),
            Some("myapp".into()),
        );
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            json,
            r#"{"event":"tunnel_ready","protocol":"http","public_url":"https://myapp.edge.rustunnel.com","local_port":3000,"local_host":"localhost","tunnel_id":"a1b2","name":"myapp"}"#
        );
    }

    #[test]
    fn tunnel_ready_tcp_derives_public_addr() {
        let ev = Event::tunnel_ready(
            "tcp",
            "tcp://edge.rustunnel.com:20001",
            5432,
            "localhost",
            None,
            None,
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "tunnel_ready");
        assert_eq!(v["public_url"], "tcp://edge.rustunnel.com:20001");
        assert_eq!(v["public_addr"], "edge.rustunnel.com:20001");
        assert!(v.get("tunnel_id").is_none());
        assert!(v.get("name").is_none());
    }

    #[test]
    fn tunnel_ready_udp_derives_public_addr() {
        let ev = Event::tunnel_ready(
            "udp",
            "udp://edge.rustunnel.com:30001",
            27015,
            "localhost",
            None,
            None,
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["public_addr"], "edge.rustunnel.com:30001");
    }

    #[test]
    fn inspector_ready_event_shape() {
        let ev = Event::InspectorReady {
            url: "http://127.0.0.1:4040".into(),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"inspector_ready","url":"http://127.0.0.1:4040"}"#
        );
    }

    #[test]
    fn notice_event_shape() {
        let ev = Event::Notice {
            message: "Hobby plan: 80% of the 1 GB monthly bandwidth is used.".into(),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"notice","message":"Hobby plan: 80% of the 1 GB monthly bandwidth is used."}"#
        );
    }

    #[test]
    fn error_event_shape() {
        let ev = Event::Error {
            code: "connection".into(),
            message: "cannot reach host".into(),
            hint: Some("check network".into()),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"error","code":"connection","message":"cannot reach host","hint":"check network"}"#
        );
    }

    #[test]
    fn error_event_omits_missing_hint() {
        let ev = Event::Error {
            code: "io".into(),
            message: "broken pipe".into(),
            hint: None,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"error","code":"io","message":"broken pipe"}"#
        );
    }

    #[test]
    fn reconnect_and_token_events() {
        let ev = Event::Reconnecting {
            attempt: 2,
            reason: "heartbeat timeout".into(),
            delay_secs: 2.0,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"reconnecting","attempt":2,"reason":"heartbeat timeout","delay_secs":2.0}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::Reconnected).unwrap(),
            r#"{"event":"reconnected"}"#
        );
        let ev = Event::TokenCreated {
            token: "tok".into(),
            name: "ci".into(),
            id: Some("42".into()),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"token_created","token":"tok","name":"ci","id":"42"}"#
        );
    }

    #[test]
    fn reconnect_pending_is_consumed_once() {
        note_reconnecting();
        assert!(take_reconnect_pending());
        assert!(!take_reconnect_pending());
    }
}
