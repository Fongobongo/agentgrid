import { useEffect, useState, Fragment } from 'react';
import { drainNode, getJson, getOpencodeAudit, listNodes, NodeView, OpencodeAuditEntry, OpencodeProfile, listOpencodeProfiles, postJson, revokeNode } from '../api';
import { ConfirmModal } from './Modal';
import { ErrorBox, Loading, StatusBadge, fmtTime, useLiveRefresh } from './util';

interface AccountUsage {
  env: string;
  token_index: number;
  attempts: number;
  rate_limited: number;
}

export default function Nodes() {
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<NodeView | null>(null);
  // Feature "opencode profiles": audit viewer expander (auto-polls with the
  // rest of the page; the WS push sequence on the CP also surfaces as a new
  // audit row immediately after the node applies it).
  const [auditNode, setAuditNode] = useState<string | null>(null);
  // Audit follow-up: audit responses raced — a slow fetch for node A could
  // resolve after node B's and render A's history under B's expanded row.
  // The rows now carry their owning node id and only render when they match.
  const [auditRows, setAuditRows] = useState<{ nodeId: string; rows: OpencodeAuditEntry[] } | null>(null);
  const [profiles, setProfiles] = useState<OpencodeProfile[] | null>(null);
  const [assignBusy, setAssignBusy] = useState<string | null>(null);
  // Enrollment token for joining a new node (POST /v1/nodes/enrollment-token).
  const [enrollToken, setEnrollToken] = useState<{ token: string; expires_at: string } | null>(null);
  // Per-node credential-pool usage (Stage: accounts/usage).
  const [usageNode, setUsageNode] = useState<string | null>(null);
  const [usageRows, setUsageRows] = useState<AccountUsage[] | null>(null);

  const load = () => {
    listNodes()
      .then((n) => {
        setError(null);
        setNodes(n);
      })
      .catch(setError);
    listOpencodeProfiles().then(setProfiles).catch(() => undefined);
    if (auditNode) {
      getOpencodeAudit(auditNode)
        .then((rows) => setAuditRows({ nodeId: auditNode, rows }))
        .catch(() => setAuditRows({ nodeId: auditNode, rows: [] }));
    }
  };

  useEffect(load, []);
  useLiveRefresh(load);

  const assignProfile = async (nodeId: string, profileId: string | null) => {
    setAssignBusy(nodeId);
    try {
      await postJson(`/v1/nodes/${nodeId}/opencode-profile`, { profile_id: profileId });
      load();
    } catch (e) {
      setError(e);
    } finally {
      setAssignBusy(null);
    }
  };

  const toggleAudit = async (nodeId: string) => {
    if (auditNode === nodeId) {
      setAuditNode(null);
      setAuditRows(null);
      return;
    }
    setAuditNode(nodeId);
    setAuditRows(null);
    try {
      const rows = await getOpencodeAudit(nodeId);
      // Drop a stale response whose node is no longer the open one.
      setAuditRows((cur) => (cur === null ? { nodeId, rows } : cur));
    } catch (e) {
      setAuditRows({ nodeId, rows: [] });
      setError(e);
    }
  };

  const revoke = async (n: NodeView) => {
    setBusy(n.id);
    try {
      const r = await revokeNode(n.id);
      if (r.ok) load();
      else setError(new Error(`Revoke failed (${r.status})`));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  // Hardening P2 item 37: toggle maintenance drain.
  const toggleDrain = async (n: NodeView) => {
    setBusy(n.id);
    try {
      const r = await drainNode(n.id, !n.drained);
      if (r.ok) load();
      else setError(new Error(`Drain failed (${r.status})`));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  const mintEnrollToken = async () => {
    try {
      const t = await postJson<{ token: string; expires_at: string }>(
        '/v1/nodes/enrollment-token',
        {},
      );
      setEnrollToken(t);
    } catch (e) {
      setError(e);
    }
  };

  const toggleUsage = async (nodeId: string) => {
    if (usageNode === nodeId) {
      setUsageNode(null);
      setUsageRows(null);
      return;
    }
    setUsageNode(nodeId);
    setUsageRows(null);
    try {
      setUsageRows(await getJson<AccountUsage[]>(`/v1/nodes/${nodeId}/accounts/usage`));
    } catch {
      setUsageRows([]);
    }
  };

  if (!nodes) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Nodes</h2>
      <p>
        <button onClick={mintEnrollToken}>Enrollment token</button>{' '}
        <span className="muted">(one-shot token a new node daemon uses at first boot)</span>
      </p>
      {enrollToken && (
        <div className="muted" style={{ fontFamily: 'monospace', fontSize: 12, wordBreak: 'break-all' }}>
          {enrollToken.token} — expires {fmtTime(enrollToken.expires_at)}{' '}
          <button onClick={() => { navigator.clipboard.writeText(enrollToken.token).catch(() => {}); }}>copy</button>{' '}
          <button onClick={() => setEnrollToken(null)}>dismiss</button>
        </div>
      )}
      {error ? <ErrorBox err={error} /> : null}
      <table className="grid">
        <thead>
          <tr>
            <th>Status</th>
            <th>Name</th>
            <th>Adapters</th>
            <th>Repositories</th>
            <th>Load</th>
            <th>Active</th>
            <th>Free disk</th>
            <th>Free mem</th>
            <th>Interception</th>
            <th>Spool</th>
            <th>Heartbeat</th>
            <th>Opencode</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => (
            <Fragment key={n.id}>
              <tr>
                <td><StatusBadge status={n.status} /></td>
                <td>
                  {n.name}
                  {/* Hardening P0 item 5: flag fully-unrestricted nodes. */}
                  {n.unsafe_active && (
                    <span className="badge err" title="Unsafe unattended mode: no sandbox, permissions bypassed">
                      ⚠ unsafe
                    </span>
                  )}
                </td>
                <td>{n.adapters.join(', ') || '—'}</td>
                <td>{n.repositories.join(', ') || '—'}</td>
                <td>{n.load_avg.toFixed(2)}</td>
                <td>{n.active_attempts}/{n.max_concurrency}</td>
                <td>{n.free_disk_mb >= 1024 ? `${(n.free_disk_mb / 1024).toFixed(1)} GB` : `${n.free_disk_mb} MB`}</td>
                <td>{(() => {
                  const m = n.mem_available_mb ?? 0;
                  if (m <= 0) return '—';
                  return m >= 1024 ? `${(m / 1024).toFixed(1)} GB` : `${m} MB`;
                })()}</td>
                <td>{n.permission_interception ?? 'wrapper'}</td>
                {/* Hardening P2 item 35: outbox + artifact spool pressure. */}
                <td>
                  {(() => {
                    const b = (n.outbox_bytes ?? 0) + (n.artifact_spool_bytes ?? 0);
                    if (b <= 0) return '—';
                    if (b >= 1048576) return `${(b / 1048576).toFixed(1)} MB`;
                    return `${b} B`;
                  })()}
                </td>
                <td>{fmtTime(n.last_heartbeat_at)}</td>
                <td>
                  {n.status !== 'revoked' && (
                    <div>
                      <select
                        disabled={assignBusy === n.id}
                        value={n.opencode_profile_id ?? ''}
                        onChange={(e) =>
                          assignProfile(n.id, e.target.value === '' ? null : e.target.value)
                        }
                        title="Bind node → opencode profile"
                      >
                        <option value="">— no profile —</option>
                        {(profiles ?? []).map((p) => (
                          <option key={p.id} value={p.id}>
                            {p.name}
                          </option>
                        ))}
                      </select>
                      {' '}
                      <button
                        onClick={() => toggleAudit(n.id)}
                        title="Show opencode-config apply history"
                      >
                        {auditNode === n.id ? 'hide audit' : 'audit'}
                      </button>
                    </div>
                  )}
                </td>
                <td>
                  {n.status !== 'revoked' && (
                    <>
                      <button
                        onClick={() => toggleUsage(n.id)}
                        title="Pool usage per account"
                      >
                        {usageNode === n.id ? 'hide usage' : 'usage'}
                      </button>{' '}
                      <button
                        disabled={busy === n.id}
                        onClick={() => toggleDrain(n)}
                        title={n.drained ? 'Undrain — allow new assignments' : 'Drain — stop new assignments (in-flight continue)'}
                      >
                        {n.drained ? 'Undrain' : 'Drain'}
                      </button>{' '}
                      <button
                        className="danger"
                        disabled={busy === n.id}
                        onClick={() => setConfirming(n)}
                      >
                        Revoke
                      </button>
                    </>
                  )}
                </td>
              </tr>
              {usageNode === n.id && (
                <tr>
                  <td colSpan={12}>
                    {usageRows === null ? (
                      <div className="muted">loading…</div>
                    ) : usageRows.length === 0 ? (
                      <div className="muted">no account-usage rows</div>
                    ) : (
                      <table className="grid">
                        <thead>
                          <tr><th>Env</th><th>Token idx</th><th>Attempts</th><th>Rate-limited</th></tr>
                        </thead>
                        <tbody>
                          {usageRows.map((u) => (
                            <tr key={u.env}>
                              <td>{u.env}</td>
                              <td>{u.token_index}</td>
                              <td>{u.attempts}</td>
                              <td>{u.rate_limited}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    )}
                  </td>
                </tr>
              )}
              {auditNode === n.id && (
                <tr>
                  <td colSpan={12}>
                    {auditRows === null || auditRows.nodeId !== n.id ? (
                      <div className="muted">loading…</div>
                    ) : auditRows.rows.length === 0 ? (
                      <div className="muted">no opencode-config applies yet</div>
                    ) : (
                      <table className="grid">
                        <thead>
                          <tr>
                            <th>At</th>
                            <th>Trigger</th>
                            <th>Profile</th>
                            <th>Hash</th>
                          </tr>
                        </thead>
                        <tbody>
                          {auditRows.rows.map((a) => (
                            <tr key={a.id}>
                              <td>{fmtTime(a.at)}</td>
                              <td>{a.trigger}</td>
                              <td>{a.profile_id ?? '—'}</td>
                              <td><code>{a.hash.slice(0, 16)}</code></td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    )}
                  </td>
                </tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>
      {confirming && (
        <ConfirmModal
          title="Revoke node"
          body={`Revoke node "${confirming.name}"? It will be denied auth immediately.`}
          confirmLabel="Revoke"
          danger
          onConfirm={() => {
            const n = confirming;
            setConfirming(null);
            revoke(n);
          }}
          onCancel={() => setConfirming(null)}
        />
      )}
    </section>
  );
}
