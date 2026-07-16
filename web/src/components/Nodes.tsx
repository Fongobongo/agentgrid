import { useEffect, useState } from 'react';
import { listNodes, NodeView, revokeNode } from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';

export default function Nodes() {
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = () => {
    listNodes().then(setNodes).catch(setError);
  };

  useEffect(load, []);

  const revoke = async (n: NodeView) => {
    if (!confirm(`Revoke node "${n.name}"? It will be denied auth immediately.`)) return;
    setRevoking(n.id);
    try {
      const r = await revokeNode(n.id);
      if (r.ok) load();
      else setError(new Error(`Revoke failed (${r.status})`));
    } catch (e) {
      setError(e);
    } finally {
      setRevoking(null);
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
            <th>Heartbeat</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => (
            <tr key={n.id}>
              <td><StatusBadge status={n.status} /></td>
              <td>{n.name}</td>
              <td>{n.adapters.join(', ') || '—'}</td>
              <td>{n.repositories.join(', ') || '—'}</td>
              <td>{n.load_avg.toFixed(2)}</td>
              <td>{n.active_attempts}/{n.max_concurrency}</td>
              <td>{n.free_disk_mb >= 1024 ? `${(n.free_disk_mb / 1024).toFixed(1)} GB` : `${n.free_disk_mb} MB`}</td>
              <td>{fmtTime(n.last_heartbeat_at)}</td>
              <td>
                {n.status !== 'revoked' && (
                  <button
                    className="danger"
                    disabled={revoking === n.id}
                    onClick={() => revoke(n)}
                  >
                    Revoke
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
