import { useEffect, useState, Fragment } from 'react';
import { drainNode, getOpencodeAudit, listNodes, NodeView, OpencodeAuditEntry, OpencodeProfile, listOpencodeProfiles, postJson, revokeNode } from '../api';
import { ConfirmModal } from './Modal';
import { ErrorBox, Loading, StatusBadge, fmtTime, useLiveRefresh } from './util';

export default function Nodes() {
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<NodeView | null>(null);
  // Feature "opencode profiles": audit viewer expander (auto-polls with the
  // rest of the page; the WS push sequence on the CP also surfaces as a new
  // audit row immediately after the node applies it).
  const [auditNode, setAuditNode] = useState<string | null>(null);
  const [auditRows, setAuditRows] = useState<OpencodeAuditEntry[] | null>(null);
  const [profiles, setProfiles] = useState<OpencodeProfile[] | null>(null);
  const [assignBusy, setAssignBusy] = useState<string | null>(null);

  const load = () => {
    listNodes()
      .then((n) => {
        setError(null);
        setNodes(n);
      })
      .catch(setError);
    listOpencodeProfiles().then(setProfiles).catch(() => undefined);
    if (auditNode) {
      getOpencodeAudit(auditNode).then(setAuditRows).catch(() => setAuditRows([]));
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
      setAuditRows(rows);
    } catch (e) {
      setAuditRows([]);
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

  if (!nodes) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Nodes</h2>
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
                      {/* Hardening P2 item 37: maintenance drain toggle. */}
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
              {auditNode === n.id && (
                <tr>
                  <td colSpan={11}>
                    {auditRows === null ? (
                      <div className="muted">loading…</div>
                    ) : auditRows.length === 0 ? (
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
                          {auditRows.map((a) => (
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
