import type { ApiClient } from './types';

export function makeApi(token: string | null): ApiClient {
  const headers: Record<string, string> = token
    ? { Authorization: `Bearer ${token}` }
    : {};

  async function get(path: string) {
    const r = await fetch(path, { headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.json();
  }

  async function del(path: string) {
    const r = await fetch(path, { method: 'DELETE', headers });
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    return r.status;
  }

  async function post(path: string, body?: unknown) {
    const r = await fetch(path, {
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
