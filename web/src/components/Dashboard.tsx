import { useEffect, useState } from 'react';
import { listNodes, listTasks, searchTasks, NodeView, TaskView } from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime, useLiveRefresh } from './util';

export default function Dashboard({ onOpen }: { onOpen: (id: string) => void }) {
  const [tasks, setTasks] = useState<TaskView[] | null>(null);
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [query, setQuery] = useState('');
  const [searchHits, setSearchHits] = useState<TaskView[] | null>(null);

  const load = () => {
    Promise.all([listTasks(), listNodes()])
      .then(([t, n]) => {
        setError(null);
        setTasks(t);
        setNodes(n);
      })
      .catch((e) => setError(e as Error));
  };

  useEffect(load, []);
  useLiveRefresh(load);

  // Plan 1.3: debounced FTS5 search — results replace the recent-tasks list
  // while a query is active.
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setSearchHits(null);
      return;
    }
    // Audit X-W5: a slow earlier query could resolve after a newer one and
    // overwrite the fresher results. The cleanup marks this run stale, so
    // only the latest in-flight response may land.
    let stale = false;
    const t = setTimeout(() => {
      searchTasks(q)
        .then((hits) => {
          if (stale) return;
          setError(null);
          setSearchHits(hits);
        })
        .catch((e) => {
          if (!stale) setError(e as Error);
        });
    }, 250);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [query]);

  if (!tasks || !nodes) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

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
      {error && <ErrorBox err={error} />}
      <div className="cards">
        {cards.map((c) => (
          <div className="card" key={c.label}>
            <div className="card-value">{c.value}</div>
            <div className="card-label">{c.label}</div>
          </div>
        ))}
      </div>

      <section>
        <h2>
          {searchHits ? `Search: ${query}` : 'Recent tasks'}
        </h2>
        <input
          className="search-input"
          type="search"
          placeholder="Search tasks (FTS5)..."
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        {searchHits
          ? (() => {
              const rows = searchHits;
              if (rows.length === 0) return <p className="muted">No tasks match.</p>;
              return (
                <table className="grid">
                  <thead>
                    <tr>
                      <th>Status</th>
                      <th>Repository</th>
                      <th>Prompt</th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((t) => (
                      <tr key={t.id} onClick={() => onOpen(t.id)} className="clickable">
                        <td><StatusBadge status={t.status} /></td>
                        <td>{t.repository}</td>
                        <td className="prompt">{t.prompt}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              );
            })()
          : completed.length === 0 && <p className="muted">No completed tasks yet.</p>}
        {!searchHits && completed.length > 0 && (
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
        )}
      </section>
    </div>
  );
}
