import { useEffect, useState } from 'react';
import {
  ApiError,
  WorkflowRun,
  WorkflowProjection,
  StepProjection,
  BudgetSnapshot,
  listWorkflowRuns,
  getWorkflowProjection,
  cancelWorkflowRun,
} from '../api';

// Stage 11.6: workflow run viewer with a DAG. Layers are computed by
// dependency depth from `depends_on`. Leaves render rightmost; the run id,
// status, and per-step verdict (role, node, attempts, error) are shown.

function statusClass(status: string): string {
  const s = status.toLowerCase();
  if (s === 'succeeded' || s === 'completed') return 'ok';
  if (s === 'failed' || s === 'cancelled') return 'err';
  if (s === 'blocked' || s === 'paused') return 'warn';
  if (s === 'running' || s === 'ready') return 'run';
  return 'idle';
}

// Assign each step a layer = 1 + max(layer of its deps); roots on layer 0.
function layers(steps: StepProjection[]): StepProjection[][] {
  const byId = new Map(steps.map((s) => [s.step_id, s]));
  const cache = new Map<string, number>();
  const depth = (id: string): number => {
    const cached = cache.get(id);
    if (cached !== undefined) return cached;
    const s = byId.get(id);
    if (!s || s.depends_on.length === 0) {
      cache.set(id, 0);
      return 0;
    }
    const d = 1 + Math.max(...s.depends_on.map((d) => depth(d)));
    cache.set(id, d);
    return d;
  };
  const cols = new Map<number, StepProjection[]>();
  for (const s of steps) {
    const d = depth(s.step_id);
    if (!cols.has(d)) cols.set(d, []);
    cols.get(d)!.push(s);
  }
  return [...cols.keys()].sort((a, b) => a - b).map((k) => cols.get(k)!);
}

function StepCard({ s }: { s: StepProjection }) {
  return (
    <div className={`wf-step ${statusClass(s.status)}`}>
      <div className="wf-head">
        <span className="wf-role">{s.role}</span>
        <span className={`wf-verdict ${statusClass(s.verdict)}`}>{s.verdict}</span>
      </div>
      <div className="wf-id" title={s.step_id}>{s.step_id}</div>
      <div className="wf-meta">
        <span className="wf-status">{s.status}</span>
        {s.attempts > 0 && <span className="wf-attempts">attempts: {s.attempts}</span>}
      </div>
      {s.node_id && <div className="wf-line">node: {s.node_id}</div>}
      {s.error_code && <div className="wf-line err">err: {s.error_code}</div>}
    </div>
  );
}

function Dag({ proj }: { proj: WorkflowProjection }) {
  const cols = layers(proj.steps);
  if (cols.length === 0) return <div className="wf-empty">no steps</div>;
  return (
    <div className="wf-dag">
      {cols.map((col, i) => (
        <div className="wf-col" key={i}>
          {col.map((s) => (
            <StepCard key={s.step_id} s={s} />
          ))}
        </div>
      ))}
    </div>
  );
}

export function WorkflowsList({ onOpen }: { onOpen: (id: string) => void }) {
  const [runs, setRuns] = useState<WorkflowRun[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = () => {
    listWorkflowRuns()
      .then(setRuns)
      .catch((e) => setErr(e instanceof ApiError ? `load failed (${e.status})` : String(e)));
  };

  useEffect(() => {
    load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, []);

  if (err) return <div className="error">{err}</div>;
  if (!runs) return <div className="loading">loading…</div>;
  if (runs.length === 0) return <div className="empty">no workflow runs</div>;

  return (
    <div className="runs">
      <h2>Workflow Runs</h2>
      <table className="runs-table">
        <thead>
          <tr>
            <th>Run</th>
            <th>Status</th>
            <th>Template</th>
            <th>Created</th>
            <th>Finished</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((r) => (
            <tr key={r.id} className="run-row" onClick={() => onOpen(r.id)}>
              <td className="mono">{r.id.slice(0, 8)}</td>
              <td className={statusClass(r.status)}>{r.status}</td>
              <td className="mono">{r.template_id.slice(0, 8)}</td>
              <td>{r.created_at}</td>
              <td>{r.finished_at ?? '—'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function WorkflowDetails({ runId }: { runId: string }) {
  const [proj, setProj] = useState<WorkflowProjection | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = () => {
    getWorkflowProjection(runId)
      .then((p) => {
        setProj(p);
        setErr(null);
      })
      .catch((e) => setErr(e instanceof ApiError ? `load failed (${e.status})` : String(e)));
  };

  useEffect(() => {
    load();
    const terminal = proj?.run.status.toLowerCase();
    const t = setInterval(load, terminal === 'succeeded' || terminal === 'failed' || terminal === 'cancelled' || terminal === 'blocked' ? 10_000 : 2000);
    return () => clearInterval(t);
  }, [runId]);

  const cancel = () => {
    cancelWorkflowRun(runId).then(load).catch((e) => setErr(String(e)));
  };

  if (err && !proj) return <div className="error">{err}</div>;
  if (!proj) return <div className="loading">loading…</div>;

  const run = proj.run;
  return (
    <div className="wf-detail">
      <div className="wf-summary">
        <h2>Run <span className="mono">{run.id.slice(0, 8)}</span></h2>
        <div className="wf-badges">
          <span className={`badge ${statusClass(run.status)}`}>{run.status}</span>
          {run.repository && <span className="badge">repo: {run.repository}</span>}
          {run.base_commit && <span className="badge mono">base: {run.base_commit.slice(0, 8)}</span>}
          {!(run.status === 'succeeded' || run.status === 'failed' || run.status === 'cancelled' || run.status === 'blocked') && (
            <button className="navbtn danger" onClick={cancel}>Cancel</button>
          )}
        </div>
        {err && <div className="error">{err}</div>}
      </div>
      {(proj.budget || run.status.toLowerCase()==="blocked") && proj.budget && <BudgetBlock snap={proj.budget} />}
      <Dag proj={proj} />
    </div>
  );
}

function BudgetBlock({ snap }: { snap: BudgetSnapshot }) {
  const limits = snap.limits;
  const rows: { name: string; lim: number; used: number }[] = (
    [
      ['max_messages', limits.max_messages ?? -1, snap.usage.messages],
      ['max_rounds', limits.max_rounds ?? -1, snap.usage.rounds],
      ['max_bytes', limits.max_bytes ?? -1, snap.usage.bytes],
      ['max_tokens', limits.max_tokens ?? -1, snap.usage.tokens],
      ['max_cost_cents', limits.max_cost_cents ?? -1, snap.usage.cost_cents],
      ['max_wall_seconds', limits.max_wall_seconds ?? -1, snap.usage.wall_seconds],
      ['max_repeated_handoffs', limits.max_repeated_handoffs ?? -1, snap.usage.repeated_handoffs],
    ] as [string, number, number][]
  )
    .filter((r) => r[1] >= 0)
    .map((r) => ({ name: r[0], lim: r[1], used: r[2] }));
  if (rows.length === 0) return null;
  return (
    <div className={`wf-budget ${snap.breach ? 'err' : ''}`}>
      <h3>Budget</h3>
      {snap.breach && (
        <div className="wf-breach">BREACH: {snap.breach.field} = {snap.breach.observed} {'>'} {snap.breach.limit}</div>
      )}
      <table className="budget-table">
        <thead><tr><th>limit</th><th>used</th></tr></thead>
        <tbody>
          {rows.map((r) => {
            const ratio = r.lim > 0 ? r.used / r.lim : 0;
            const over = r.used > r.lim;
            return (
              <tr key={r.name} className={over ? 'err' : ratio > 0.8 ? 'warn' : ''}>
                <td className="mono">{r.name}</td>
                <td className="mono">{r.used} {'/'} {r.lim}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
