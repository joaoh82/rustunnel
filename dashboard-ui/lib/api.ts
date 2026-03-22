import type { ApiClient } from './types';

// Default base URL of the rustunnel-server dashboard API.
// Set NEXT_PUBLIC_API_URL in .env.local for dev, and as a Vercel env var for production.
// Defaults to empty string (same-origin) for local dev when running behind a proxy.
const DEFAULT_API_BASE = process.env.NEXT_PUBLIC_API_URL ?? '';

/**
 * Create an API client targeting the given server.
 *
 * @param token  Bearer token for authentication (null = unauthenticated).
 * @param base   Optional override for the API base URL (defaults to NEXT_PUBLIC_API_URL).
 *               Pass the result of `regionApiUrl(region)` to target a specific regional server.
 */
export function makeApi(token: string | null, base?: string): ApiClient {
  const API_BASE = base ?? DEFAULT_API_BASE;
  const headers: Record<string, string> = token
    ? { Authorization: `Bearer ${token}` }
    : {};

  async function get(path: string) {
    const r = await fetch(`${API_BASE}${path}`, { headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.json();
  }

  async function del(path: string) {
    const r = await fetch(`${API_BASE}${path}`, { method: 'DELETE', headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.status;
  }

  async function post(path: string, body?: unknown) {
    const r = await fetch(`${API_BASE}${path}`, {
      method: 'POST',
      headers: body ? { ...headers, 'Content-Type': 'application/json' } : headers,
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.json();
  }

  return { get, del, post };
}

export function statusColor(code: number | undefined): string {
  if (!code) return 'var(--muted)';
  if (code < 300) return 'var(--green)';
  if (code < 500) return 'var(--yellow)';
  return 'var(--red)';
}

export function relativeTime(iso: string | undefined): string {
  if (!iso) return '—';
  const diff = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

export function prettyJson(str: string | null | undefined): string | null {
  if (!str) return null;
  try {
    return JSON.stringify(JSON.parse(str), null, 2);
  } catch {
    return str;
  }
}

export function copyToClipboard(text: string): void {
  navigator.clipboard.writeText(text).catch(() => {});
}
