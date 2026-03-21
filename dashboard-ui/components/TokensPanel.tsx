'use client';

import { useState } from 'react';
import type { ApiClient, ApiToken, CreateTokenResponse } from '@/lib/types';
import { relativeTime, copyToClipboard } from '@/lib/api';
import { Panel } from './Panel';
import { HistoryTable } from './TunnelHistoryPanel';

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
  isExpanded: boolean;
  onToggle: (id: string) => void;
}

function TokenRow({ token, onDelete, isDeleting, isExpanded, onToggle }: TokenRowProps) {
  return (
    <tr
      onClick={() => onToggle(token.id)}
      style={{
        borderBottom: isExpanded ? 'none' : '1px solid var(--border)',
        cursor: 'pointer',
        background: isExpanded ? 'var(--surface, rgba(255,255,255,0.03))' : undefined,
        transition: 'background 0.1s',
      }}
      onMouseEnter={(e) => {
        if (!isExpanded) (e.currentTarget as HTMLTableRowElement).style.background = 'var(--surface, rgba(255,255,255,0.03))';
      }}
      onMouseLeave={(e) => {
        if (!isExpanded) (e.currentTarget as HTMLTableRowElement).style.background = '';
      }}
    >
      <td style={{ padding: '10px 14px', maxWidth: 200 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <span style={{ fontSize: 10, color: 'var(--muted)', transition: 'transform 0.15s', display: 'inline-block', transform: isExpanded ? 'rotate(90deg)' : 'none' }}>
            ▶
          </span>
          <span style={{ color: 'var(--text)', fontWeight: 500 }}>{token.label}</span>
        </div>
      </td>
      <td style={{ padding: '10px 14px', color: 'var(--muted)', fontFamily: 'var(--mono)', fontSize: 11 }}>
        {token.scope ?? <span style={{ opacity: 0.4 }}>unrestricted</span>}
      </td>
      <td style={{ padding: '10px 14px', fontFamily: 'var(--mono)', fontSize: 12, whiteSpace: 'nowrap' }}>
        {token.tunnel_count > 0
          ? <span style={{ color: 'var(--text)' }}>{token.tunnel_count.toLocaleString()}</span>
          : <span style={{ color: 'var(--muted)' }}>0</span>}
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
      <td
        style={{ padding: '10px 14px' }}
        onClick={(e) => e.stopPropagation()} // Don't toggle expand when clicking delete
      >
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

// ── Token drill-down row ───────────────────────────────────────────────────────

interface DrillDownRowProps {
  token: ApiToken;
  api: ApiClient;
  colSpan: number;
}

function DrillDownRow({ token, api, colSpan }: DrillDownRowProps) {
  return (
    <tr style={{ borderBottom: '1px solid var(--border)' }}>
      <td colSpan={colSpan} style={{ padding: 0 }}>
        <div
          style={{
            background: 'var(--surface, rgba(255,255,255,0.02))',
            borderLeft: '3px solid var(--border)',
            marginLeft: 8,
          }}
        >
          <div style={{ padding: '6px 16px 4px', fontSize: 11, color: 'var(--muted)' }}>
            Tunnels for <strong style={{ color: 'var(--text)' }}>{token.label}</strong>
            <code style={{ marginLeft: 8, fontSize: 10 }}>{token.id.slice(0, 8)}…</code>
          </div>
          <HistoryTable api={api} enabled={true} tokenId={token.id} compact />
        </div>
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
  const [expandedId, setExpandedId] = useState<string | null>(null);

  async function handleDelete(id: string) {
    setDeleting(id);
    if (expandedId === id) setExpandedId(null);
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

  function toggleExpand(id: string) {
    setExpandedId((cur) => (cur === id ? null : id));
  }

  const colSpan = 7;

  return (
    <Panel
      title={`API Tokens${tokens.length > 0 ? ` (${tokens.length})` : ''}`}
      actions={<span style={{ fontSize: 11, color: 'var(--muted)' }}>click a row to view tunnels</span>}
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
      </div>

      {tokens.length === 0 ? (
        <div style={{ padding: '24px 16px', textAlign: 'center', color: 'var(--muted)', fontSize: 13 }}>
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
                {['Label', 'Scope', 'Tunnels', 'Created', 'Last Used', 'ID', ''].map((h) => (
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
                <>
                  <TokenRow
                    key={t.id}
                    token={t}
                    onDelete={handleDelete}
                    isDeleting={deleting === t.id}
                    isExpanded={expandedId === t.id}
                    onToggle={toggleExpand}
                  />
                  {expandedId === t.id && (
                    <DrillDownRow key={`${t.id}-detail`} token={t} api={api} colSpan={colSpan} />
                  )}
                </>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}
