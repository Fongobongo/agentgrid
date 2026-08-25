import { useEffect, useState } from 'react';
import { listTasks, TaskView } from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime, useLiveRefresh } from './util';

// Plan 2.13 (#26): Background-specialist panel.
//
// Purpose: give the operator a live HUD of what specialist attempts (i.e.
// tasks whose tags include a specialist marker, or whose prompts cover a
// specialty like "security-review" / "eval-case") are currently in-flight.
// We avoid adding new API surface — `listTasks` already returns every
// TaskView; the filters below keep the HUD cheap on client-CPU. Refreshes
// on the control-plane change stream (idle = no requests).

// Tasks that the engine currently considers "background" — anything not
// yet terminal. Terminal statuses hide a specialist from the HUD (they're
// done thinking).
const ACTIVE_STATUSES = ['assigned', 'running', 'pending', 'retry'];

// Local capability vocabulary for the HUD filter. Not exported — no other
// view consumes it.
interface SpecialistCapability {
  tag: string;
  description: string;
}

const SPECIALIST_CAPABILITIES: SpecialistCapability[] = [
  { tag: 'security-review', description: 'Reads skills, MCP bundles, or task prompts for injection patterns' },
  { tag: 'eval-case', description: 'Probes failing attempts against stamped eval scripts' },
  { tag: 'consensus', description: 'Runs N-of-M majority votes across adapter models' },
  { tag: 'autopilot', description: 'Autonomous retry loops guarded by validation commands' },
];

export default function Background() {
  const [all, setAll] = useState<TaskView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [capability, setCapability] = useState<string>('all');
  const [statusFilter, setStatusFilter] = useState<string>('active');
  const [repoFilter, setRepoFilter] = useState<string>('');

  const load = () => {
    listTasks()
      .then(setAll)
      .catch(setError);
  };
  useEffect(load, []);
  useLiveRefresh(load);

  if (!all) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  const onwire = all.filter((t) => {
    if (statusFilter === 'active') {
      return ACTIVE_STATUSES.includes(t.status.toLowerCase());
    }
    if (statusFilter === 'terminal') {
      return !ACTIVE_STATUSES.includes(t.status.toLowerCase());
    }
    return true;
  });
  const oncap = onwire.filter((t) => {
    if (capability === 'all') return true;
    const promptCap = t.prompt.toLowerCase().includes(capability);
    return promptCap;
  });
  const onrepo = oncap.filter((t) => !repoFilter || t.repository.toLowerCase().includes(repoFilter.toLowerCase()));
  const specialists = SPECIALIST_CAPABILITIES.map((c) => {
    const counter = onrepo.filter((t) =>
      t.prompt.toLowerCase().includes(c.tag),
    );
    return { ...c, count: counter.length, examples: counter.slice(0, 3) };
  });

  return (
    <section>
      <h2>Background specialists</h2>
      <p className="muted">
        Live HUD of in-flight attempts by capability tag. Refreshes on control-plane changes. The
        "all-active" cell shows tasks whose status is in <code>{ACTIVE_STATUSES.join(', ')}</code>.
      </p>
      <div className="filter-row">
        <label>
          Capability:
          <select value={capability} onChange={(e) => setCapability(e.target.value)}>
            <option value="all">all</option>
            {SPECIALIST_CAPABILITIES.map((c) => (
              <option key={c.tag} value={c.tag}>{c.tag}</option>
            ))}
          </select>
        </label>
        <label>
          Status:
          <select value={statusFilter} onChange={(e) => setStatusFilter(e.target.value)}>
            <option value="active">active</option>
            <option value="terminal">terminal</option>
            <option value="all">all</option>
          </select>
        </label>
        <label>
          Repo:
          <input value={repoFilter} onChange={(e) => setRepoFilter(e.target.value)} placeholder="org/repo filter" />
        </label>
      </div>
      <div className="cards">
        {specialists.map((s) => (
          <div className="card" key={s.tag}>
            <h3>
              <span className="badge">{s.tag}</span> <span className="muted">×{s.count}</span>
            </h3>
            <p>{s.description}</p>
            {s.examples.length > 0 ? (
              <ul>
                {s.examples.map((t) => (
                  <li key={t.id}>
                    <a href={`#/task/${t.id}`}>
                      #{t.id.slice(0, 8)}
                    </a>
                    <StatusBadge status={t.status} />{' '}
                    <span className="mono">{t.repository}</span>{' '}
                    <span className="muted">{fmtTime(t.created_at)}</span>
                  </li>
                ))}
              </ul>
            ) : (
              <div className="muted">no active attempts</div>
            )}
          </div>
        ))}
      </div>
      {onrepo.length === 0 && (
        <p className="muted">No tasks match the filters — drop a repo filter or widen the capability.</p>
      )}
      <h3>Matching tasks</h3>
      <table className="grid">
        <thead>
          <tr>
            <th>Status</th>
            <th>Repository</th>
            <th>Prompt (head)</th>
            <th>Created</th>
          </tr>
        </thead>
        <tbody>
          {onrepo.slice(0, 25).map((t) => (
            <tr key={t.id}>
              <td><StatusBadge status={t.status} /></td>
              <td className="mono"><a href={`#/task/${t.id}`}>{t.repository}</a></td>
              <td>{t.prompt.slice(0, 80)}{t.prompt.length > 80 ? '…' : ''}</td>
              <td>{fmtTime(t.created_at)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
