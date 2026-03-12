# REST API Reference

The rustunnel-server exposes a REST API on the **dashboard port** (default `8443` in Docker, `4041` in local dev, `8080` in bare-metal production — check your `server.toml`).

---

## Authentication

All endpoints except `GET /api/status` require an `Authorization` header:

```
Authorization: Bearer <token>
```

Two token types are accepted:

| Type | Value | Notes |
|------|-------|-------|
| Admin token | Value of `auth.admin_token` in `server.toml` | Full access |
| API token | Raw UUID returned at token creation time | Same access as admin token today; scope enforcement planned |

**Error responses**

```json
{ "error": "missing token" }      // 401 — no Authorization header
{ "error": "invalid token" }      // 401 — token not recognised
```

---

## Endpoints

### Status

#### `GET /api/status`

Health check. Does **not** require authentication.

**Response `200`**

```json
{
  "ok": true,
  "active_sessions": 3,
  "active_tunnels": 5
}
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | Always `true` when the server is healthy |
| `active_sessions` | integer | Number of connected clients |
| `active_tunnels` | integer | Total active HTTP + TCP tunnels |

**Example**

```bash
curl http://localhost:4041/api/status
```

---

### Tunnels

#### `GET /api/tunnels`

List all currently active tunnels.

**Response `200`** — array of tunnel objects

```json
[
  {
    "tunnel_id": "a1b2c3d4-...",
    "protocol": "http",
    "label": "abc123.localhost",
    "public_url": "https://abc123.localhost",
    "connected_since": "2025-06-01T12:00:00Z",
    "request_count": 42,
    "client_addr": "192.168.1.10:54321"
  },
  {
    "tunnel_id": "e5f6a7b8-...",
    "protocol": "tcp",
    "label": "20001",
    "public_url": "tcp://:20001",
    "connected_since": "2025-06-01T12:05:00Z",
    "request_count": 7,
    "client_addr": "192.168.1.10:54322"
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `tunnel_id` | string (UUID) | Unique tunnel identifier |
| `protocol` | `"http"` \| `"tcp"` | Tunnel type |
| `label` | string | Subdomain (HTTP) or port number (TCP) |
| `public_url` | string | Full public URL of the tunnel |
| `connected_since` | string (ISO-8601 UTC) | When the tunnel was registered |
| `request_count` | integer | Cumulative proxied requests / connections |
| `client_addr` | string | IP:port of the client that opened the tunnel |

**Example**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:4041/api/tunnels
```

---

#### `GET /api/tunnels/:id`

Retrieve a single active tunnel by its UUID.

**Path parameters**

| Parameter | Description |
|-----------|-------------|
| `id` | Tunnel UUID |

**Response `200`** — single tunnel object (same shape as the array items above)

**Response `404`**

```json
{ "error": "tunnel not found" }
```

**Example**

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4041/api/tunnels/a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

---

#### `DELETE /api/tunnels/:id`

Force-close an active tunnel and remove it from the routing table.

**Path parameters**

| Parameter | Description |
|-----------|-------------|
| `id` | Tunnel UUID |

**Response `204`** — tunnel removed

**Response `404`**

```json
{ "error": "invalid tunnel id" }
```

**Example**

```bash
curl -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://localhost:4041/api/tunnels/a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

---

#### `GET /api/tunnels/:id/requests`

List recently captured HTTP requests that passed through a tunnel. Results are served from an in-memory ring buffer (last 500 per tunnel) when the tunnel is active, and from SQLite otherwise.

**Path parameters**

| Parameter | Description |
|-----------|-------------|
| `id` | Tunnel UUID (string, not the subdomain) |

**Query parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `limit` | integer | `50` | Maximum number of requests to return |

**Response `200`** — array of captured request objects

```json
[
  {
    "id": "req-uuid-...",
    "tunnel_id": "a1b2c3d4-...",
    "conn_id": "conn-uuid-...",
    "method": "GET",
    "path": "/api/users?page=1",
    "status": 200,
    "request_bytes": 312,
    "response_bytes": 1024,
    "duration_ms": 45,
    "captured_at": "2025-06-01T12:01:00Z",
    "request_body": null,
    "response_body": null
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string (UUID) | Unique request identifier |
| `tunnel_id` | string (UUID) | Owning tunnel |
| `conn_id` | string (UUID) | Connection identifier |
| `method` | string | HTTP method (`GET`, `POST`, etc.) |
| `path` | string | Request path including query string |
| `status` | integer | HTTP response status code |
| `request_bytes` | integer | Size of the request body in bytes |
| `response_bytes` | integer | Size of the response body in bytes |
| `duration_ms` | integer | Round-trip proxy time in milliseconds |
| `captured_at` | string (ISO-8601 UTC) | When the request was captured |
| `request_body` | string \| null | Stored request body (JSON); `null` if not captured |
| `response_body` | string \| null | Stored response body (JSON); `null` if not captured |

**Example**

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:4041/api/tunnels/a1b2c3d4-.../requests?limit=20"
```

---

#### `POST /api/tunnels/:id/replay/:request_id`

Retrieve a stored request for replay. Returns the full captured request record including body.

> **Note**: This endpoint returns the stored payload for inspection. Sending the actual replay HTTP request to the upstream service is left to the client.

**Path parameters**

| Parameter | Description |
|-----------|-------------|
| `id` | Tunnel UUID |
| `request_id` | Captured request UUID (from `GET /api/tunnels/:id/requests`) |

**Response `200`** — full captured request object (same shape as above, body fields populated)

**Response `404`**

```json
{ "error": "request not found" }
{ "error": "request does not belong to this tunnel" }
```

**Example**

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4041/api/tunnels/a1b2c3d4-.../replay/req-uuid-...
```

---

### Tokens

#### `GET /api/tokens`

List all API tokens with their tunnel usage counts. The raw token value is never returned after creation — only the SHA-256 hash is stored.

**Response `200`** — array of token objects

```json
[
  {
    "id": "tok-uuid-...",
    "token_hash": "e3b0c44298fc...",
    "label": "ci-deploy",
    "created_at": "2025-05-15T09:00:00Z",
    "last_used_at": "2025-06-01T11:55:00Z",
    "scope": null,
    "tunnel_count": 14
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string (UUID) | Token identifier |
| `token_hash` | string | SHA-256 hex digest of the raw token |
| `label` | string | Human-readable name |
| `created_at` | string (ISO-8601 UTC) | Creation timestamp |
| `last_used_at` | string \| null | Last successful authentication timestamp |
| `scope` | string \| null | Comma-separated subdomain patterns; `null` = unrestricted |
| `tunnel_count` | integer | Total tunnels ever registered with this token |

**Example**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:4041/api/tokens
```

---

#### `POST /api/tokens`

Create a new API token. The raw token value is returned **only in this response** and cannot be recovered afterwards.

**Request body** (`application/json`)

```json
{
  "label": "ci-deploy",
  "scope": null
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `label` | string | yes | Human-readable name for the token |
| `scope` | string \| null | no | Comma-separated subdomain patterns to restrict this token. `null` or omitted = unrestricted |

**Response `201`**

```json
{
  "id": "tok-uuid-...",
  "label": "ci-deploy",
  "token": "977ebf87-88f3-4af0-9ead-0426a3d00ecd"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string (UUID) | Token identifier (use this with `DELETE /api/tokens/:id`) |
| `label` | string | The label provided in the request |
| `token` | string | **Raw token value — store this securely, shown only once** |

**Example**

```bash
curl -X POST \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"label": "ci-deploy"}' \
  http://localhost:4041/api/tokens
```

---

#### `DELETE /api/tokens/:id`

Delete an API token. Any client currently connected with this token will be disconnected on the next auth check.

**Path parameters**

| Parameter | Description |
|-----------|-------------|
| `id` | Token UUID (the `id` field, not the raw token value) |

**Response `204`** — token deleted

**Response `404`**

```json
{ "error": "token not found" }
```

**Example**

```bash
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  http://localhost:4041/api/tokens/tok-uuid-...
```

---

### History

#### `GET /api/history`

Paginated log of all tunnel registrations (both active and past). Newest entries are returned first.

**Query parameters**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `limit` | integer | `50` | Page size (max records per response) |
| `offset` | integer | `0` | Number of records to skip (for pagination) |
| `protocol` | `"http"` \| `"tcp"` | — | Filter to a specific protocol; omit for all |

**Response `200`**

```json
{
  "total": 128,
  "entries": [
    {
      "id": "log-uuid-...",
      "tunnel_id": "a1b2c3d4-...",
      "protocol": "http",
      "label": "abc123.tunnel.example.com",
      "session_id": "sess-uuid-...",
      "token_id": "tok-uuid-...",
      "token_label": "ci-deploy",
      "registered_at": "2025-06-01T12:00:00Z",
      "unregistered_at": "2025-06-01T12:45:00Z"
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `total` | integer | Total records matching the filter (for pagination) |
| `entries[].id` | string (UUID) | Log record identifier |
| `entries[].tunnel_id` | string (UUID) | Tunnel identifier |
| `entries[].protocol` | `"http"` \| `"tcp"` | Tunnel type |
| `entries[].label` | string | Subdomain (HTTP) or port number (TCP) |
| `entries[].session_id` | string (UUID) | Client session that registered this tunnel |
| `entries[].token_id` | string \| null | API token that was used; `null` for admin-token sessions |
| `entries[].token_label` | string \| null | Human-readable label of the token; `null` for admin sessions |
| `entries[].registered_at` | string (ISO-8601 UTC) | When the tunnel was opened |
| `entries[].unregistered_at` | string \| null | When the tunnel was closed; `null` if still active |

**Pagination example** — page 3 of HTTP tunnels, 25 per page:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:4041/api/history?protocol=http&limit=25&offset=50"
```

---

## Error responses

All error responses share the same shape:

```json
{ "error": "<human-readable message>" }
```

| Status | Meaning |
|--------|---------|
| `200` | Success |
| `201` | Resource created |
| `204` | Success, no body (DELETE) |
| `401` | Missing or invalid token |
| `404` | Resource not found |
| `500` | Internal server error |

---

## Base URL

| Environment | Base URL |
|-------------|----------|
| Local dev (bare metal) | `http://localhost:4041` |
| Local dev (Docker) | `http://localhost:4041` |
| Production | `https://<your-domain>:8443` |

The dashboard port is set by `dashboard_port` in `server.toml`.
