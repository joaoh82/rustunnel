'use client';

import { useState, useEffect, useMemo } from 'react';
import type { Tunnel, CapturedRequest, Region } from '@/lib/types';
import { makeApi } from '@/lib/api';
import { loadRegions, regionApiUrl } from '@/lib/regions';
import { useTunnels } from '@/hooks/useTunnels';
import { useRequests } from '@/hooks/useRequests';
import { useRegionHealth } from '@/hooks/useRegionHealth';
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
  const [regions, setRegions] = useState<Region[]>([]);

  // Read token from localStorage on mount (avoids SSR mismatch).
  useEffect(() => {
    setToken(localStorage.getItem('rt_token'));
  }, []);

  // Load region list once on mount.
  useEffect(() => {
    loadRegions().then(setRegions);
  }, []);

  // One API client per region, keyed by region ID.
  const regionApis = useMemo(
    () => regions.map((r) => ({ regionId: r.id, api: makeApi(token, regionApiUrl(r)) })),
    [regions, token]
  );

  // Fallback single-region API (used for history, tokens, replay).
  const primaryApi = useMemo(() => makeApi(token), [token]);

  // Per-region health polling.
  const regionHealth = useRegionHealth(regions);

  // Active tunnels — fanned out across all regions.
  const { tunnels, error: tunnelErr, refresh: refreshTunnels } = useTunnels(regionApis, !!token);

  const [selectedTunnel, setSelectedTunnel] = useState<Tunnel | null>(null);
  const [selectedRequest, setSelectedRequest] = useState<CapturedRequest | null>(null);
  const [replayResult, setReplayResult] = useState<string | null>(null);

  // API client for the selected tunnel's region (for request inspector and replay).
  const selectedTunnelApi = useMemo(() => {
    if (!selectedTunnel?.region_id) return primaryApi;
    const match = regionApis.find((r) => r.regionId === selectedTunnel.region_id);
    return match?.api ?? primaryApi;
  }, [selectedTunnel, regionApis, primaryApi]);

  const { requests } = useRequests(selectedTunnelApi, selectedTunnel?.tunnel_id ?? null);
  const { tokens, error: tokenErr, refresh: refreshTokens } = useTokens(primaryApi, !!token);

  // Deselect tunnel if it disappears.
  useEffect(() => {
    if (selectedTunnel && !tunnels.find((t) => t.tunnel_id === selectedTunnel.tunnel_id)) {
      setSelectedTunnel(null);
      setSelectedRequest(null);
    }
  }, [tunnels, selectedTunnel]);

  async function handleClose(tunnel: Tunnel) {
    // Route the delete to the correct regional server.
    const api = regionApis.find((r) => r.regionId === tunnel.region_id)?.api ?? primaryApi;
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
      await selectedTunnelApi.post(
        `/api/tunnels/${selectedTunnel!.tunnel_id}/replay/${req.id}`
      );
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
    return <AuthGate onAuth={setToken} />;
  }

  return (
    <>
      <Header regions={regions} regionHealth={regionHealth} onSignOut={signOut} />

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
          api={primaryApi}
          tokens={tokens}
          error={tokenErr}
          refresh={refreshTokens}
        />

        {/* Tunnel history — uses primary API (shared PostgreSQL returns all regions) */}
        <TunnelHistoryPanel api={primaryApi} enabled={!!token} />
      </main>
    </>
  );
}
