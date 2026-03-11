'use client';

import { useState } from 'react';
import type { ApiClient, ApiToken, CreateTokenResponse } from '@/lib/types';
import { relativeTime, copyToClipboard } from '@/lib/api';
import { Panel } from './Panel';

// ── NewTokenBanner ─────────────────────────────────────────────────────────────

interface NewTokenBannerProps {
  result: CreateTokenResponse;
  onDismiss: () => void;
}

function NewTokenBanner({ result, onDismiss }: NewTokenBannerProps) {
  const [copied, setCopied] = useState(false);
  return (
    <div
      style={{
        marginBottom: 16,
        padding: '14px 16px',
        background: '#0d2a1a',
        border: '1px solid #1e5e30',
        borderRadius: 'var(--radius)',
        fontSize: 12,
        fontFamily: 'var(--mono)',
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
        <span style={{ color: 'var(--green)', fontWeight: 600 }}>
          ✓ Token created — copy it now, it won&apos;t be shown again.
        </span>
        <button onClick={onDismiss} style={{ padding: '2px 8px', fontSize: 11 }}>✕</button>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <code
          style={{
            flex: 1,
            padding: '8px 12px',
            background: 'var(--bg)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius)',
            color: 'var(--text)',
            overflowX: 'auto',
            whiteSpace: 'nowrap',
            fontSize: 12,
          }}
        >
          {result.token}
        </code>
        <button
          style={{ padding: '6px 12px', fontSize: 11, flexShrink: 0 }}
          onClick={() => {
            copyToClipboard(result.token);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          }}
        >
          {copied ? 'Copied!' : 'Copy'}
        </button>
      </div>
      <div style={{ marginTop: 8, color: 'var(--muted)', fontSize: 11 }}>
        Label: <span style={{ color: 'var(--text)' }}>{result.label}</span>
        &nbsp;·&nbsp;ID: {result.id}
      </div>
    </div>
  );
}

// ── CreateTokenForm ─────────────────────────────────────────────────────────────

interface CreateTokenFormProps {
  onCreated: (result: CreateTokenResponse) => void;
  api: ApiClient;
}

function CreateTokenForm({ onCreated, api }: CreateTokenFormProps) {
  const [label, setLabel] = useState('');
  const [scope, setScope] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!label.trim()) return;
    setLoading(true);
    setError(null);
    try {
      const body: Record<string, string> = { label: label.trim() };
      if (scope.trim()) body.scope = scope.trim();
      const result = await api.post('/api/tokens', body);
      onCreated(result as CreateTokenResponse);
      setLabel('');
      setScope('');
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }

  return (
    <form
      onSubmit={handleSubmit}
      style={{
        display: 'flex',
        gap: 8,
        alignItems: 'flex-end',
        flexWrap: 'wrap',
        marginBottom: 16,
        paddingBottom: 16,
        borderBottom: '1px solid var(--border)',
      }}
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <label style={{ fontSize: 11, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
          Label *
        </label>
        <input
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder="e.g. ci-deploy"
          required
          style={{ width: 180 }}
        />
      </div>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <label style={{ fontSize: 11, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
          Scope <span style={{ opacity: 0.5 }}>(optional)</span>
        </label>
        <input
          value={scope}
          onChange={(e) => setScope(e.target.value)}
          placeholder="e.g. abc,def"
          style={{ width: 160 }}
        />
      </div>
      <button type="submit" disabled={loading || !label.trim()} style={{ padding: '6px 14px' }}>
        {loading ? 'Creating…' : '+ Create Token'}
      </button>
      {error && (
        <span style={{ fontSize: 11, color: 'var(--red)', alignSelf: 'center' }}>
          {error}
        </span>
      )}
    </form>
  );
}

// ── TokenRow ────────────────────────────────────────────────────────────────────

interface TokenRowProps {
  token: ApiToken;
  onDelete: (id: string) => void;
  isDeleting: boolean;
}

function TokenRow({ token, onDelete, isDeleting }: TokenRowProps) {
  return (
    <tr style={{ borderBottom: '1px solid var(--border)' }}>
      <td style={{ padding: '10px 14px', maxWidth: 200 }}>
        <span style={{ color: 'var(--text)', fontWeight: 500 }}>{token.label}</span>
      </td>
      <td style={{ padding: '10px 14px', color: 'var(--muted)', fontFamily: 'var(--mono)', fontSize: 11 }}>
        {token.scope ?? <span style={{ opacity: 0.4 }}>unrestricted</span>}
      </td>
      <td style={{ padding: '10px 14px', color: 'var(--muted)', fontFamily: 'var(--mono)', fontSize: 11, whiteSpace: 'nowrap' }}>
        {relativeTime(token.created_at)}
      </td>
      <td style={{ padding: '10px 14px', color: 'var(--muted)', fontFamily: 'var(--mono)', fontSize: 11, whiteSpace: 'nowrap' }}>
        {token.last_used_at ? relativeTime(token.last_used_at) : <span style={{ opacity: 0.4 }}>never</span>}
      </td>
      <td style={{ padding: '10px 14px', color: 'var(--muted)', fontFamily: 'var(--mono)', fontSize: 11 }}>
        <span title={token.id}>{token.id.slice(0, 8)}…</span>
      </td>
      <td style={{ padding: '10px 14px' }}>
        <button
          className="danger"
          style={{ padding: '3px 8px', fontSize: 11 }}
          disabled={isDeleting}
          onClick={() => {
            if (confirm(`Delete token "${token.label}"? Any clients using it will lose access.`)) {
              onDelete(token.id);
            }
          }}
        >
          Delete
        </button>
      </td>
    </tr>
  );
}

// ── TokensPanel ─────────────────────────────────────────────────────────────────

interface TokensPanelProps {
  api: ApiClient;
  tokens: ApiToken[];
  error: string | null;
  refresh: () => void;
}

export function TokensPanel({ api, tokens, error, refresh }: TokensPanelProps) {
  const [newToken, setNewToken] = useState<CreateTokenResponse | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  async function handleDelete(id: string) {
    setDeleting(id);
    try {
      await api.del(`/api/tokens/${id}`);
      refresh();
    } catch (e) {
      alert(`Failed to delete token: ${(e as Error).message}`);
    } finally {
      setDeleting(null);
    }
  }

  function handleCreated(result: CreateTokenResponse) {
    setNewToken(result);
    refresh();
  }

  return (
    <Panel
      title={`API Tokens${tokens.length > 0 ? ` (${tokens.length})` : ''}`}
      actions={<span style={{ fontSize: 11, color: 'var(--muted)' }}>auto-refresh 5s</span>}
    >
      <div style={{ padding: '14px 16px' }}>
        {newToken && (
          <NewTokenBanner result={newToken} onDismiss={() => setNewToken(null)} />
        )}

        <CreateTokenForm api={api} onCreated={handleCreated} />

        {error && (
          <div style={{ marginBottom: 12, fontSize: 12, color: 'var(--red)' }}>
            Failed to load tokens: {error}
          </div>
        )}

        {tokens.length === 0 ? (
          <div style={{ padding: '24px 0', textAlign: 'center', color: 'var(--muted)', fontSize: 13 }}>
            No API tokens yet.
          </div>
        ) : (
          <div style={{ overflowX: 'auto' }}>
            <table
              style={{
                width: '100%',
                borderCollapse: 'collapse',
                fontSize: 12,
                fontFamily: 'var(--mono)',
              }}
            >
              <thead>
                <tr style={{ borderBottom: '1px solid var(--border)', color: 'var(--muted)' }}>
                  {['Label', 'Scope', 'Created', 'Last Used', 'ID', ''].map((h) => (
                    <th
                      key={h}
                      style={{
                        padding: '7px 14px',
                        textAlign: 'left',
                        fontWeight: 500,
                        fontSize: 11,
                        textTransform: 'uppercase',
                        letterSpacing: '0.05em',
                        whiteSpace: 'nowrap',
                      }}
                    >
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {tokens.map((t) => (
                  <TokenRow
                    key={t.id}
                    token={t}
                    onDelete={handleDelete}
                    isDeleting={deleting === t.id}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </Panel>
  );
}
