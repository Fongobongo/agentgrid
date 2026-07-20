import { useEffect, useState } from 'react';
import {
  ApiError,
  WorkflowRun,
  WorkflowProjection,
  StepProjection,
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
      <Dag proj={proj} />
    </div>
  );
}
