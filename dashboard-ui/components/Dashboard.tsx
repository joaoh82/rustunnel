'use client';

import { useState, useEffect, useMemo } from 'react';
import type { Tunnel, CapturedRequest } from '@/lib/types';
import { makeApi } from '@/lib/api';
import { useServerStatus } from '@/hooks/useServerStatus';
import { useTunnels } from '@/hooks/useTunnels';
import { useRequests } from '@/hooks/useRequests';
import { AuthGate } from './AuthGate';
import { Header } from './Header';
import { Panel } from './Panel';
import { TunnelTable } from './TunnelTable';
import { RequestList } from './RequestList';
import { RequestDetail } from './RequestDetail';
import { TokensPanel } from './TokensPanel';
import { TunnelHistoryPanel } from './TunnelHistoryPanel';
import { useTokens } from '@/hooks/useTokens';

export default function Dashboard() {
  const [token, setToken] = useState<string | null>(null);

  // Read token from localStorage on mount (avoids SSR mismatch).
  useEffect(() => {
    setToken(localStorage.getItem('rt_token'));
  }, []);

  const api = useMemo(() => makeApi(token), [token]);
  const status = useServerStatus();
  const { tunnels, error: tunnelErr, refresh: refreshTunnels } = useTunnels(api, !!token);
  const [selectedTunnel, setSelectedTunnel] = useState<Tunnel | null>(null);
  const [selectedRequest, setSelectedRequest] = useState<CapturedRequest | null>(null);
  const [replayResult, setReplayResult] = useState<string | null>(null);

  const { requests } = useRequests(api, selectedTunnel?.tunnel_id ?? null);
  const { tokens, error: tokenErr, refresh: refreshTokens } = useTokens(api, !!token);

  // Deselect tunnel if it disappears.
  useEffect(() => {
    if (selectedTunnel && !tunnels.find((t) => t.tunnel_id === selectedTunnel.tunnel_id)) {
      setSelectedTunnel(null);
      setSelectedRequest(null);
    }
  }, [tunnels, selectedTunnel]);

  async function handleClose(tunnel: Tunnel) {
    try {
      await api.del(`/api/tunnels/${tunnel.tunnel_id}`);
      if (selectedTunnel?.tunnel_id === tunnel.tunnel_id) {
        setSelectedTunnel(null);
        setSelectedRequest(null);
      }
      refreshTunnels();
    } catch (e) {
      alert(`Failed to close tunnel: ${(e as Error).message}`);
    }
  }

  async function handleReplay(req: CapturedRequest) {
    try {
      await api.post(`/api/tunnels/${selectedTunnel!.tunnel_id}/replay/${req.id}`);
      setReplayResult(req.id);
      setTimeout(() => setReplayResult(null), 3000);
    } catch (e) {
      alert(`Replay failed: ${(e as Error).message}`);
    }
  }

  function signOut() {
    localStorage.removeItem('rt_token');
    setToken(null);
  }

  // Show auth gate until we know the token (or it's null after mount).
  if (token === null) {
    // token is null both before mount (SSR) and when unauthenticated.
    // AuthGate handles the sign-in flow.
    return <AuthGate onAuth={setToken} />;
  }

  return (
    <>
      <Header status={status} onSignOut={signOut} />

      <main
        style={{
          padding: '16px 20px',
          display: 'flex',
          flexDirection: 'column',
          gap: 14,
          maxWidth: 1400,
          margin: '0 auto',
        }}
      >
        {/* Auth error */}
        {tunnelErr && tunnelErr.includes('401') && (
          <div
            style={{
              padding: '10px 14px',
              background: '#2a1212',
              border: '1px solid #5a1f1f',
              borderRadius: 'var(--radius)',
              color: 'var(--red)',
              fontSize: 12,
            }}
          >
            Authentication failed — your token may have expired.{' '}
            <button className="danger" style={{ marginLeft: 8 }} onClick={signOut}>
              Sign Out
            </button>
          </div>
        )}

        {/* Active tunnels */}
        <Panel
          title={`Active Tunnels${tunnels.length > 0 ? ` (${tunnels.length})` : ''}`}
          actions={
            <span style={{ fontSize: 11, color: 'var(--muted)' }}>auto-refresh 2s</span>
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
              <button
                style={{ padding: '3px 8px', fontSize: 11 }}
                onClick={() => setSelectedTunnel(null)}
              >
                ✕
              </button>
            }
          >
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: selectedRequest ? '1fr 1fr' : '1fr',
                minHeight: 280,
                overflow: 'hidden',
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
          <div
            style={{ color: 'var(--muted)', fontSize: 12, textAlign: 'center', padding: '8px 0' }}
          >
            Click a tunnel row to inspect its requests.
          </div>
        )}

        {/* API token management */}
        <TokensPanel
          api={api}
          tokens={tokens}
          error={tokenErr}
          refresh={refreshTokens}
        />

        {/* Tunnel history */}
        <TunnelHistoryPanel api={api} enabled={!!token} />
      </main>
    </>
  );
}
