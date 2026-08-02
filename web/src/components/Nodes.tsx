import { useEffect, useState } from 'react';
import { drainNode, listNodes, NodeView, revokeNode } from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';

export default function Nodes() {
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = () => {
    listNodes().then(setNodes).catch(setError);
  };

  useEffect(load, []);

  const revoke = async (n: NodeView) => {
    if (!confirm(`Revoke node "${n.name}"? It will be denied auth immediately.`)) return;
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

  if (error) return <ErrorBox err={error} />;
  if (!nodes) return <Loading />;

  return (
    <section>
      <h2>Nodes</h2>
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
            <th></th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => (
            <tr key={n.id}>
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
                      onClick={() => revoke(n)}
                    >
                      Revoke
                    </button>
                  </>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
