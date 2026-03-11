import { useState, useEffect, useCallback, useRef } from "react";

// ── theme ─────────────────────────────────────────────────────────────────────

const CSS = `
  :root {
    --bg:        #0d0d0d;
    --surface:   #161616;
    --surface2:  #1e1e1e;
    --border:    #2a2a2a;
    --border2:   #333;
    --text:      #e8e8e8;
    --muted:     #666;
    --accent:    #3b82f6;
    --accent-dim:#1e3a5f;
    --green:     #22c55e;
    --red:       #ef4444;
    --yellow:    #f59e0b;
    --purple:    #a78bfa;
    --mono:      "JetBrains Mono", "Fira Code", "Cascadia Code", monospace;
    --sans:      -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    --radius:    6px;
  }

  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }

  body {
    background: var(--bg);
    color: var(--text);
    font-family: var(--sans);
    font-size: 13px;
    line-height: 1.5;
    min-height: 100vh;
  }

  button {
    cursor: pointer;
    font-family: inherit;
    font-size: 12px;
    border: 1px solid var(--border2);
    border-radius: var(--radius);
    background: var(--surface2);
    color: var(--text);
    padding: 4px 10px;
    transition: background 0.15s, border-color 0.15s;
  }
  button:hover { background: #2a2a2a; border-color: #444; }
  button:active { background: #333; }
  button.primary {
    background: var(--accent-dim);
    border-color: var(--accent);
    color: var(--accent);
  }
  button.primary:hover { background: #1a3f72; }
  button.danger {
    background: #2a1212;
    border-color: #5a1f1f;
    color: var(--red);
  }
  button.danger:hover { background: #3a1818; }

  input {
    font-family: var(--mono);
    font-size: 13px;
    background: var(--surface2);
    border: 1px solid var(--border2);
    border-radius: var(--radius);
    color: var(--text);
    padding: 7px 10px;
    outline: none;
    transition: border-color 0.15s;
  }
  input:focus { border-color: var(--accent); }

  a { color: var(--accent); text-decoration: none; }
  a:hover { text-decoration: underline; }

  pre, code { font-family: var(--mono); }

  ::-webkit-scrollbar { width: 6px; height: 6px; }
  ::-webkit-scrollbar-track { background: transparent; }
  ::-webkit-scrollbar-thumb { background: var(--border2); border-radius: 3px; }
  ::-webkit-scrollbar-thumb:hover { background: #444; }
`;

// ── helpers ───────────────────────────────────────────────────────────────────

function statusColor(code) {
  if (!code) return "var(--muted)";
  if (code < 300) return "var(--green)";
  if (code < 400) return "var(--yellow)";
  if (code < 500) return "var(--yellow)";
  return "var(--red)";
}

function relativeTime(iso) {
  if (!iso) return "—";
  const diff = Math.floor((Date.now() - new Date(iso)) / 1000);
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function prettyJson(str) {
  if (!str) return null;
  try {
    return JSON.stringify(JSON.parse(str), null, 2);
  } catch {
    return str;
  }
}

function copyToClipboard(text) {
  navigator.clipboard.writeText(text).catch(() => {});
}

// ── API client ────────────────────────────────────────────────────────────────

function makeApi(token) {
  const headers = token ? { Authorization: `Bearer ${token}` } : {};

  async function get(path) {
    const r = await fetch(path, { headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.json();
  }

  async function del(path) {
    const r = await fetch(path, { method: "DELETE", headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.status;
  }

  async function post(path) {
    const r = await fetch(path, { method: "POST", headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.json();
  }

  return { get, del, post };
}

// ── hooks ─────────────────────────────────────────────────────────────────────

function useInterval(fn, ms) {
  const saved = useRef(fn);
  useEffect(() => { saved.current = fn; }, [fn]);
  useEffect(() => {
    const id = setInterval(() => saved.current(), ms);
    return () => clearInterval(id);
  }, [ms]);
}

function useServerStatus() {
  const [status, setStatus] = useState(null);
  const poll = useCallback(async () => {
    try {
      const s = await fetch("/api/status").then((r) => r.json());
      setStatus(s);
    } catch {
      setStatus(null);
    }
  }, []);
  useEffect(() => { poll(); }, [poll]);
  useInterval(poll, 2000);
  return status;
}

function useTunnels(api, enabled) {
  const [tunnels, setTunnels] = useState([]);
  const [error, setError] = useState(null);
  const poll = useCallback(async () => {
    if (!enabled) return;
    try {
      const data = await api.get("/api/tunnels");
      setTunnels(data);
      setError(null);
    } catch (e) {
      setError(e.message);
    }
  }, [api, enabled]);
  useEffect(() => { poll(); }, [poll]);
  useInterval(poll, 2000);
  return { tunnels, error, refresh: poll };
}

function useRequests(api, tunnelId) {
  const [requests, setRequests] = useState([]);
  const poll = useCallback(async () => {
    if (!tunnelId) return;
    try {
      const data = await api.get(`/api/tunnels/${tunnelId}/requests?limit=100`);
      setRequests(data);
    } catch {
      setRequests([]);
    }
  }, [api, tunnelId]);
  useEffect(() => { poll(); }, [poll]);
  useInterval(poll, 2000);
  return { requests, refresh: poll };
}

// ── components ────────────────────────────────────────────────────────────────

function Dot({ color, size = 8, pulse }) {
  return (
    <span
      style={{
        display: "inline-block",
        width: size,
        height: size,
        borderRadius: "50%",
        background: color,
        flexShrink: 0,
        boxShadow: pulse ? `0 0 6px ${color}` : "none",
        animation: pulse ? "pulse 2s infinite" : "none",
      }}
    />
  );
}

function Badge({ label, color = "var(--muted)" }) {
  return (
    <span
      style={{
        display: "inline-block",
        padding: "1px 7px",
        borderRadius: 99,
        fontSize: 11,
        fontFamily: "var(--mono)",
        background: color + "22",
        color,
        border: `1px solid ${color}44`,
      }}
    >
      {label}
    </span>
  );
}

// ── AuthGate ──────────────────────────────────────────────────────────────────

function AuthGate({ onAuth }) {
  const [val, setVal] = useState("");
  const [err, setErr] = useState(null);
  const [loading, setLoading] = useState(false);

  async function submit(e) {
    e.preventDefault();
    if (!val.trim()) return;
    setLoading(true);
    try {
      await fetch("/api/tunnels", {
        headers: { Authorization: `Bearer ${val.trim()}` },
      }).then((r) => {
        if (!r.ok) throw new Error("Unauthorized");
        return r.json();
      });
      localStorage.setItem("rt_token", val.trim());
      onAuth(val.trim());
    } catch {
      setErr("Invalid token — check your admin_token in server.toml.");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        minHeight: "100vh",
        background: "var(--bg)",
      }}
    >
      <div
        style={{
          background: "var(--surface)",
          border: "1px solid var(--border)",
          borderRadius: 10,
          padding: "36px 40px",
          width: 380,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 24 }}>
          <span style={{ fontSize: 24 }}>🔗</span>
          <span style={{ fontSize: 18, fontWeight: 600, letterSpacing: "-0.3px" }}>
            Rustunnel Dashboard
          </span>
        </div>
        <form onSubmit={submit}>
          <label style={{ display: "block", marginBottom: 6, color: "var(--muted)", fontSize: 12 }}>
            API TOKEN
          </label>
          <input
            type="password"
            placeholder="Enter your admin token…"
            value={val}
            onChange={(e) => { setVal(e.target.value); setErr(null); }}
            style={{ width: "100%", marginBottom: 12 }}
            autoFocus
          />
          {err && (
            <div style={{ color: "var(--red)", fontSize: 12, marginBottom: 10 }}>{err}</div>
          )}
          <button
            type="submit"
            className="primary"
            style={{ width: "100%", padding: "8px" }}
            disabled={loading}
          >
            {loading ? "Checking…" : "Sign In"}
          </button>
        </form>
      </div>
    </div>
  );
}

// ── Header ────────────────────────────────────────────────────────────────────

function Header({ status, onSignOut }) {
  const ok = status?.ok === true;
  return (
    <header
      style={{
        display: "flex",
        alignItems: "center",
        padding: "0 20px",
        height: 50,
        background: "var(--surface)",
        borderBottom: "1px solid var(--border)",
        gap: 12,
        position: "sticky",
        top: 0,
        zIndex: 100,
      }}
    >
      <span style={{ fontSize: 18 }}>🔗</span>
      <span style={{ fontWeight: 600, fontSize: 14, letterSpacing: "-0.2px", marginRight: "auto" }}>
        Rustunnel Dashboard
      </span>

      {status && (
        <div style={{ display: "flex", alignItems: "center", gap: 14, color: "var(--muted)", fontSize: 12 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <Dot color={ok ? "var(--green)" : "var(--red)"} pulse={ok} />
            <span style={{ color: ok ? "var(--green)" : "var(--red)" }}>
              {ok ? "Healthy" : "Offline"}
            </span>
          </div>
          <span title="Active sessions">{status.active_sessions} session{status.active_sessions !== 1 ? "s" : ""}</span>
          <span title="Active tunnels">{status.active_tunnels} tunnel{status.active_tunnels !== 1 ? "s" : ""}</span>
        </div>
      )}

      <button onClick={onSignOut} style={{ marginLeft: 8 }}>
        Sign Out
      </button>
    </header>
  );
}

// ── TunnelTable ───────────────────────────────────────────────────────────────

function TunnelTable({ tunnels, selected, onSelect, onClose }) {
  if (tunnels.length === 0) {
    return (
      <div
        style={{
          padding: "60px 20px",
          textAlign: "center",
          color: "var(--muted)",
          fontSize: 13,
        }}
      >
        <div style={{ fontSize: 32, marginBottom: 10 }}>⟳</div>
        No active tunnels — connect a client to get started.
      </div>
    );
  }

  return (
    <div style={{ overflowX: "auto" }}>
      <table
        style={{
          width: "100%",
          borderCollapse: "collapse",
          fontSize: 12,
          fontFamily: "var(--mono)",
        }}
      >
        <thead>
          <tr style={{ borderBottom: "1px solid var(--border)", color: "var(--muted)" }}>
            {["Protocol", "Public URL", "Client", "Connected", "Requests", ""].map((h) => (
              <th
                key={h}
                style={{
                  padding: "8px 14px",
                  textAlign: "left",
                  fontWeight: 500,
                  fontSize: 11,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  whiteSpace: "nowrap",
                }}
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {tunnels.map((t) => {
            const isSelected = selected?.tunnel_id === t.tunnel_id;
            return (
              <tr
                key={t.tunnel_id}
                onClick={() => onSelect(isSelected ? null : t)}
                style={{
                  borderBottom: "1px solid var(--border)",
                  cursor: "pointer",
                  background: isSelected ? "var(--accent-dim)" : "transparent",
                  transition: "background 0.1s",
                }}
                onMouseEnter={(e) => {
                  if (!isSelected) e.currentTarget.style.background = "var(--surface2)";
                }}
                onMouseLeave={(e) => {
                  if (!isSelected) e.currentTarget.style.background = "transparent";
                }}
              >
                <td style={{ padding: "10px 14px" }}>
                  <Badge
                    label={t.protocol.toUpperCase()}
                    color={t.protocol === "http" ? "var(--accent)" : "var(--purple)"}
                  />
                </td>
                <td style={{ padding: "10px 14px", color: "var(--accent)", maxWidth: 260 }}>
                  <span
                    style={{ cursor: "pointer" }}
                    title={t.public_url}
                    onClick={(e) => {
                      e.stopPropagation();
                      copyToClipboard(t.public_url);
                    }}
                  >
                    {t.public_url}
                  </span>
                </td>
                <td style={{ padding: "10px 14px", color: "var(--muted)" }}>{t.client_addr || "—"}</td>
                <td style={{ padding: "10px 14px", color: "var(--muted)", whiteSpace: "nowrap" }}>
                  {relativeTime(t.connected_since)}
                </td>
                <td style={{ padding: "10px 14px" }}>
                  {t.request_count > 0 ? (
                    <span style={{ color: "var(--text)" }}>{t.request_count.toLocaleString()}</span>
                  ) : (
                    <span style={{ color: "var(--muted)" }}>0</span>
                  )}
                </td>
                <td style={{ padding: "10px 14px" }}>
                  <button
                    className="danger"
                    style={{ padding: "3px 8px", fontSize: 11 }}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (confirm(`Force close tunnel ${t.label}?`)) onClose(t);
                    }}
                  >
                    ✕ Close
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ── RequestList ───────────────────────────────────────────────────────────────

function RequestList({ requests, selectedId, onSelect, onReplay }) {
  if (requests.length === 0) {
    return (
      <div style={{ padding: "40px 20px", textAlign: "center", color: "var(--muted)" }}>
        No requests captured yet.
      </div>
    );
  }

  return (
    <div style={{ overflowX: "auto" }}>
      <table
        style={{
          width: "100%",
          borderCollapse: "collapse",
          fontSize: 12,
          fontFamily: "var(--mono)",
        }}
      >
        <thead>
          <tr style={{ borderBottom: "1px solid var(--border)", color: "var(--muted)" }}>
            {["Time", "Method", "Path", "Status", "Duration", "Size", ""].map((h) => (
              <th
                key={h}
                style={{
                  padding: "7px 12px",
                  textAlign: "left",
                  fontWeight: 500,
                  fontSize: 11,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  whiteSpace: "nowrap",
                }}
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {requests.map((r) => {
            const isSelected = selectedId === r.id;
            const methodColor = {
              GET: "var(--green)", POST: "var(--accent)", PUT: "var(--yellow)",
              PATCH: "var(--yellow)", DELETE: "var(--red)", OPTIONS: "var(--muted)",
            }[r.method] ?? "var(--text)";
            return (
              <tr
                key={r.id}
                onClick={() => onSelect(isSelected ? null : r)}
                style={{
                  borderBottom: "1px solid var(--border)",
                  cursor: "pointer",
                  background: isSelected ? "#1a2535" : "transparent",
                  transition: "background 0.1s",
                }}
                onMouseEnter={(e) => {
                  if (!isSelected) e.currentTarget.style.background = "var(--surface2)";
                }}
                onMouseLeave={(e) => {
                  if (!isSelected) e.currentTarget.style.background = "transparent";
                }}
              >
                <td style={{ padding: "8px 12px", color: "var(--muted)", whiteSpace: "nowrap" }}>
                  {new Date(r.captured_at).toLocaleTimeString()}
                </td>
                <td style={{ padding: "8px 12px", color: methodColor, fontWeight: 600 }}>
                  {r.method}
                </td>
                <td
                  style={{
                    padding: "8px 12px",
                    maxWidth: 300,
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                  title={r.path}
                >
                  {r.path}
                </td>
                <td style={{ padding: "8px 12px", color: statusColor(r.status) }}>
                  {r.status || "—"}
                </td>
                <td style={{ padding: "8px 12px", color: "var(--muted)", whiteSpace: "nowrap" }}>
                  {r.duration_ms != null ? `${r.duration_ms}ms` : "—"}
                </td>
                <td style={{ padding: "8px 12px", color: "var(--muted)", whiteSpace: "nowrap" }}>
                  {r.response_bytes != null
                    ? r.response_bytes > 1024
                      ? `${(r.response_bytes / 1024).toFixed(1)}k`
                      : `${r.response_bytes}b`
                    : "—"}
                </td>
                <td style={{ padding: "8px 12px" }}>
                  <button
                    style={{ padding: "2px 7px", fontSize: 11 }}
                    onClick={(e) => {
                      e.stopPropagation();
                      onReplay(r);
                    }}
                    title="Replay this request"
                  >
                    ↺ Replay
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ── RequestDetail ─────────────────────────────────────────────────────────────

function CodeBlock({ title, content, lang }) {
  const [copied, setCopied] = useState(false);
  if (!content) return null;
  const pretty = lang === "json" ? prettyJson(content) ?? content : content;
  return (
    <div style={{ marginBottom: 16 }}>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: 6,
        }}
      >
        <span style={{ fontSize: 11, color: "var(--muted)", textTransform: "uppercase", letterSpacing: "0.05em" }}>
          {title}
        </span>
        <button
          style={{ padding: "2px 7px", fontSize: 10 }}
          onClick={() => {
            copyToClipboard(pretty);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          }}
        >
          {copied ? "Copied!" : "Copy"}
        </button>
      </div>
      <pre
        style={{
          background: "var(--bg)",
          border: "1px solid var(--border)",
          borderRadius: "var(--radius)",
          padding: "12px 14px",
          fontSize: 12,
          overflowX: "auto",
          maxHeight: 300,
          overflowY: "auto",
          color: "var(--text)",
          lineHeight: 1.6,
          whiteSpace: "pre-wrap",
          wordBreak: "break-all",
        }}
      >
        {pretty}
      </pre>
    </div>
  );
}

function RequestDetail({ request, onClose, replayResult }) {
  const req = request;
  const methodColor = {
    GET: "var(--green)", POST: "var(--accent)", PUT: "var(--yellow)",
    PATCH: "var(--yellow)", DELETE: "var(--red)",
  }[req.method] ?? "var(--text)";

  // Parse stored JSON blobs (request_body / response_body may be JSON strings
  // that themselves contain a headers+body object).
  let reqHeaders = null, reqBody = null, resHeaders = null, resBody = null;
  try {
    const rb = req.request_body ? JSON.parse(req.request_body) : null;
    if (rb && typeof rb === "object" && "headers" in rb) {
      reqHeaders = JSON.stringify(rb.headers, null, 2);
      reqBody = typeof rb.body === "string" ? rb.body : JSON.stringify(rb.body, null, 2);
    } else {
      reqBody = req.request_body;
    }
  } catch { reqBody = req.request_body; }

  try {
    const rb = req.response_body ? JSON.parse(req.response_body) : null;
    if (rb && typeof rb === "object" && "headers" in rb) {
      resHeaders = JSON.stringify(rb.headers, null, 2);
      resBody = typeof rb.body === "string" ? rb.body : JSON.stringify(rb.body, null, 2);
    } else {
      resBody = req.response_body;
    }
  } catch { resBody = req.response_body; }

  return (
    <div
      style={{
        padding: "16px 20px",
        borderTop: "1px solid var(--border)",
        background: "var(--surface)",
        overflowY: "auto",
        flex: 1,
      }}
    >
      {/* Title bar */}
      <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 16 }}>
        <span style={{ fontFamily: "var(--mono)", fontWeight: 700, color: methodColor }}>
          {req.method}
        </span>
        <span
          style={{
            fontFamily: "var(--mono)",
            color: "var(--text)",
            flex: 1,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {req.path}
        </span>
        <span style={{ fontFamily: "var(--mono)", color: statusColor(req.status) }}>
          {req.status}
        </span>
        <span style={{ color: "var(--muted)", fontSize: 12 }}>{req.duration_ms}ms</span>
        <button onClick={onClose} style={{ marginLeft: 4, padding: "2px 8px" }}>✕</button>
      </div>

      {/* Meta row */}
      <div
        style={{
          display: "flex",
          gap: 20,
          marginBottom: 16,
          fontSize: 11,
          color: "var(--muted)",
          fontFamily: "var(--mono)",
          flexWrap: "wrap",
        }}
      >
        <span>id: {req.id}</span>
        <span>captured: {new Date(req.captured_at).toLocaleString()}</span>
        <span>req: {req.request_bytes}b</span>
        <span>res: {req.response_bytes}b</span>
      </div>

      {/* Replay result banner */}
      {replayResult && (
        <div
          style={{
            marginBottom: 14,
            padding: "8px 12px",
            background: "#0d2a1a",
            border: "1px solid #1e5e30",
            borderRadius: "var(--radius)",
            fontSize: 12,
            color: "var(--green)",
            fontFamily: "var(--mono)",
          }}
        >
          ↺ Replay queued — tunnel will forward this request again.
        </div>
      )}

      <CodeBlock title="Request Headers" content={reqHeaders} lang="json" />
      <CodeBlock title="Request Body" content={reqBody} lang="json" />
      <CodeBlock title="Response Headers" content={resHeaders} lang="json" />
      <CodeBlock title="Response Body" content={resBody} lang="json" />

      {!reqHeaders && !reqBody && !resHeaders && !resBody && (
        <div style={{ color: "var(--muted)", fontSize: 12 }}>
          No body captured for this request.
        </div>
      )}
    </div>
  );
}

// ── Panel ─────────────────────────────────────────────────────────────────────

function Panel({ title, children, actions }) {
  return (
    <div
      style={{
        background: "var(--surface)",
        border: "1px solid var(--border)",
        borderRadius: 8,
        overflow: "hidden",
        display: "flex",
        flexDirection: "column",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          padding: "10px 16px",
          borderBottom: "1px solid var(--border)",
          gap: 10,
        }}
      >
        <span style={{ fontWeight: 600, fontSize: 12, color: "var(--muted)", textTransform: "uppercase", letterSpacing: "0.05em", flex: 1 }}>
          {title}
        </span>
        {actions}
      </div>
      <div style={{ flex: 1, overflow: "auto" }}>{children}</div>
    </div>
  );
}

// ── App ───────────────────────────────────────────────────────────────────────

export default function App() {
  const [token, setToken] = useState(() => localStorage.getItem("rt_token") || null);
  const api = token ? makeApi(token) : makeApi(null);

  const status = useServerStatus();
  const { tunnels, error: tunnelErr, refresh: refreshTunnels } = useTunnels(api, !!token);
  const [selectedTunnel, setSelectedTunnel] = useState(null);
  const [selectedRequest, setSelectedRequest] = useState(null);
  const [replayResult, setReplayResult] = useState(null);

  const { requests } = useRequests(
    api,
    selectedTunnel?.tunnel_id ?? null
  );

  // Deselect tunnel if it disappears.
  useEffect(() => {
    if (selectedTunnel && !tunnels.find((t) => t.tunnel_id === selectedTunnel.tunnel_id)) {
      setSelectedTunnel(null);
      setSelectedRequest(null);
    }
  }, [tunnels, selectedTunnel]);

  async function handleClose(tunnel) {
    try {
      await api.del(`/api/tunnels/${tunnel.tunnel_id}`);
      if (selectedTunnel?.tunnel_id === tunnel.tunnel_id) {
        setSelectedTunnel(null);
        setSelectedRequest(null);
      }
      refreshTunnels();
    } catch (e) {
      alert(`Failed to close tunnel: ${e.message}`);
    }
  }

  async function handleReplay(req) {
    try {
      await api.post(`/api/tunnels/${selectedTunnel.tunnel_id}/replay/${req.id}`);
      setReplayResult(req.id);
      setTimeout(() => setReplayResult(null), 3000);
    } catch (e) {
      alert(`Replay failed: ${e.message}`);
    }
  }

  function signOut() {
    localStorage.removeItem("rt_token");
    setToken(null);
  }

  if (!token) {
    return (
      <>
        <style>{CSS}</style>
        <AuthGate onAuth={setToken} />
      </>
    );
  }

  return (
    <>
      <style>{CSS + `
        @keyframes pulse {
          0%, 100% { opacity: 1; }
          50%       { opacity: 0.4; }
        }
      `}</style>

      <Header status={status} onSignOut={signOut} />

      <main
        style={{
          padding: "16px 20px",
          display: "flex",
          flexDirection: "column",
          gap: 14,
          maxWidth: 1400,
          margin: "0 auto",
        }}
      >
        {/* Auth error */}
        {tunnelErr && tunnelErr.includes("401") && (
          <div
            style={{
              padding: "10px 14px",
              background: "#2a1212",
              border: "1px solid #5a1f1f",
              borderRadius: "var(--radius)",
              color: "var(--red)",
              fontSize: 12,
            }}
          >
            Authentication failed — your token may have expired.{" "}
            <button className="danger" style={{ marginLeft: 8 }} onClick={signOut}>
              Sign Out
            </button>
          </div>
        )}

        {/* Active tunnels */}
        <Panel
          title={`Active Tunnels ${tunnels.length > 0 ? `(${tunnels.length})` : ""}`}
          actions={
            <span style={{ fontSize: 11, color: "var(--muted)" }}>auto-refresh 2s</span>
          }
        >
          <TunnelTable
            tunnels={tunnels}
            selected={selectedTunnel}
            onSelect={setSelectedTunnel}
            onClose={handleClose}
          />
        </Panel>

        {/* Request inspector */}
        {selectedTunnel && (
          <Panel
            title={`Requests — ${selectedTunnel.public_url}`}
            actions={
              <button style={{ padding: "3px 8px", fontSize: 11 }} onClick={() => setSelectedTunnel(null)}>
                ✕
              </button>
            }
          >
            <div
              style={{
                display: "grid",
                gridTemplateColumns: selectedRequest ? "1fr 1fr" : "1fr",
                minHeight: 280,
                overflow: "hidden",
              }}
            >
              <RequestList
                requests={requests}
                selectedId={selectedRequest?.id}
                onSelect={setSelectedRequest}
                onReplay={handleReplay}
              />
              {selectedRequest && (
                <RequestDetail
                  request={selectedRequest}
                  onClose={() => setSelectedRequest(null)}
                  replayResult={replayResult === selectedRequest.id ? true : null}
                />
              )}
            </div>
          </Panel>
        )}

        {!selectedTunnel && (
          <div style={{ color: "var(--muted)", fontSize: 12, textAlign: "center", padding: "8px 0" }}>
            Click a tunnel row to inspect its requests.
          </div>
        )}
      </main>
    </>
  );
}
