import { useEffect, useState } from 'react';
import { ApiError, listNodes, listTasks, NodeView, TaskView } from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';

export default function Dashboard({ onOpen }: { onOpen: (id: string) => void }) {
  const [tasks, setTasks] = useState<TaskView[] | null>(null);
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<Error | null>(null);

  const load = () => {
    Promise.all([listTasks(), listNodes()])
      .then(([t, n]) => {
        setTasks(t);
        setNodes(n);
      })
      .catch((e) => setError(e as Error));
  };

  useEffect(load, []);

  if (error) return <ErrorBox err={error} />;
  if (!tasks || !nodes) return <Loading />;

  const nodeByStatus: Record<string, number> = {};
  for (const n of nodes) nodeByStatus[n.status] = (nodeByStatus[n.status] ?? 0) + 1;

  const running = tasks.filter((t) => ['assigned', 'running', 'validating'].includes(t.status)).length;
  const queued = tasks.filter((t) => t.status === 'queued').length;
  const completed = tasks
    .filter((t) => ['succeeded', 'failed', 'cancelled'].includes(t.status))
    .sort((a, b) => (b.finished_at ?? '').localeCompare(a.finished_at ?? ''))
    .slice(0, 10);

  const cards = [
    { label: 'Nodes online', value: nodeByStatus['online'] ?? 0 },
    { label: 'Nodes total', value: nodes.length },
    { label: 'Tasks running', value: running },
    { label: 'Tasks queued', value: queued },
  ];

  return (
    <div className="dashboard">
      <div className="cards">
        {cards.map((c) => (
          <div className="card" key={c.label}>
            <div className="card-value">{c.value}</div>
            <div className="card-label">{c.label}</div>
          </div>
        ))}
      </div>

      <section>
        <h2>Recent tasks</h2>
        {completed.length === 0 && <p className="muted">No completed tasks yet.</p>}
        <table className="grid">
          <thead>
            <tr>
              <th>Status</th>
              <th>Repository</th>
              <th>Prompt</th>
              <th>Finished</th>
            </tr>
          </thead>
          <tbody>
            {completed.map((t) => (
              <tr key={t.id} onClick={() => onOpen(t.id)} className="clickable">
                <td><StatusBadge status={t.status} /></td>
                <td>{t.repository}</td>
                <td className="prompt">{t.prompt}</td>
                <td>{fmtTime(t.finished_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  );
}

export { ApiError };
