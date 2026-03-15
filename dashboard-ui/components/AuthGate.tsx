'use client';

import { useState } from 'react';

const API_BASE = process.env.NEXT_PUBLIC_API_URL ?? '';

interface AuthGateProps {
  onAuth: (token: string) => void;
}

export function AuthGate({ onAuth }: AuthGateProps) {
  const [val, setVal] = useState('');
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!val.trim()) return;
    setLoading(true);
    try {
      await fetch(`${API_BASE}/api/tunnels`, {
        headers: { Authorization: `Bearer ${val.trim()}` },
      }).then((r) => {
        if (!r.ok) throw new Error('Unauthorized');
        return r.json();
      });
      localStorage.setItem('rt_token', val.trim());
      onAuth(val.trim());
    } catch {
      setErr('Invalid token — check your admin_token in server.toml.');
    } finally {
      setLoading(false);
    }
  }

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        minHeight: '100vh',
        background: 'var(--bg)',
      }}
    >
      <div
        style={{
          background: 'var(--surface)',
          border: '1px solid var(--border)',
          borderRadius: 10,
          padding: '36px 40px',
          width: 380,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 24 }}>
          <span style={{ fontSize: 24 }}>🔗</span>
          <span style={{ fontSize: 18, fontWeight: 600, letterSpacing: '-0.3px' }}>
            Rustunnel Dashboard
          </span>
        </div>
        <form onSubmit={submit}>
          <label style={{ display: 'block', marginBottom: 6, color: 'var(--muted)', fontSize: 12 }}>
            API TOKEN
          </label>
          <input
            type="password"
            placeholder="Enter your admin token…"
            value={val}
            onChange={(e) => { setVal(e.target.value); setErr(null); }}
            style={{ width: '100%', marginBottom: 12 }}
            autoFocus
          />
          {err && (
            <div style={{ color: 'var(--red)', fontSize: 12, marginBottom: 10 }}>{err}</div>
          )}
          <button
            type="submit"
            className="primary"
            style={{ width: '100%', padding: '8px' }}
            disabled={loading}
          >
            {loading ? 'Checking…' : 'Sign In'}
          </button>
        </form>
      </div>
    </div>
  );
}
