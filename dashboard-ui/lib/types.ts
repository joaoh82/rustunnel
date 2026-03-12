export interface Tunnel {
  tunnel_id: string;
  protocol: string;
  label: string;
  public_url: string;
  connected_since: string;
  request_count: number;
  client_addr: string | null;
}

export interface CapturedRequest {
  id: string;
  tunnel_id: string;
  conn_id: string;
  method: string;
  path: string;
  status: number;
  request_bytes: number;
  response_bytes: number;
  duration_ms: number;
  captured_at: string;
  request_body: string | null;
  response_body: string | null;
}

export interface ServerStatus {
  ok: boolean;
  active_sessions: number;
  active_tunnels: number;
}

export interface ApiToken {
  id: string;
  label: string;
  token_hash: string;
  created_at: string;
  last_used_at: string | null;
  scope: string | null;
  tunnel_count: number;
}

export interface CreateTokenResponse {
  id: string;
  label: string;
  token: string; // raw value — shown only once
}

export interface TunnelLogEntry {
  id: string;
  tunnel_id: string;
  protocol: string;
  label: string;
  session_id: string;
  token_id: string | null;
  token_label: string | null;
  registered_at: string;
  unregistered_at: string | null;
}

export interface TunnelHistoryResponse {
  entries: TunnelLogEntry[];
  total: number;
}

export interface ApiClient {
  get: (path: string) => Promise<unknown>;
  del: (path: string) => Promise<number>;
  post: (path: string, body?: unknown) => Promise<unknown>;
}
