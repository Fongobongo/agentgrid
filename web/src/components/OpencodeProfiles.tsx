import { useEffect, useState } from 'react';
import { getJson, postJson } from '../api';
import { ErrorBox, Loading, fmtTime } from './util';

// Feature "opencode profiles": control-plane-hosted opencode configuration —
// list every stored profile, view one in detail, paste a config body to
// upsert (PUT), assign it to a node (POST /v1/nodes/:id/opencode-profile),
// delete. Polls — the apply states refresh as nodes pull over the WS push.

interface OpencodeProfile {
  id: string;
  name: string;
  hash: string;
  config: Record<string, unknown>;
  prev?: { hash: string; config: Record<string, unknown> } | null;
  expires_at?: string | null;
  created_at: string;
  updated_at: string;
}

interface NodeView {
  id: string;
  name: string;
  opencode_profile_id: string | null;
}

const POLL_MS = 5000;

// Line diff between two pretty-printed JSON configs. Small inputs
// (tens of lines), so an O(n*m) LCS is fine and dependency-free.
type DiffLine = { t: 'same' | 'add' | 'del'; s: string };
function diffLines(a: string[], b: string[]): DiffLine[] {
  const n = a.length;
  const m = b.length;
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ t: 'same', s: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ t: 'del', s: a[i] });
      i++;
    } else {
      out.push({ t: 'add', s: b[j] });
      j++;
    }
  }
  while (i < n) out.push({ t: 'del', s: a[i++] });
  while (j < m) out.push({ t: 'add', s: b[j++] });
  return out;
}

function ConfigDiff({ prev, cur }: { prev: Record<string, unknown>; cur: Record<string, unknown> }) {
  const lines = diffLines(
    JSON.stringify(prev, null, 2).split('\n'),
    JSON.stringify(cur, null, 2).split('\n'),
  );
  const changed = lines.filter((l) => l.t !== 'same').length;
  return (
    <>
      <div className="muted">{changed} changed line{changed === 1 ? '' : 's'}</div>
      <pre className="diff">
        {lines.map((l, i) => (
          <div key={i} className={`dline ${l.t}`}>
            {l.t === 'del' ? '-' : l.t === 'add' ? '+' : ' '} {l.s}
          </div>
        ))}
      </pre>
    </>
  );
}

export default function OpencodeProfiles() {
  const [profiles, setProfiles] = useState<OpencodeProfile[] | null>(null);
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [error, setError] = useState<unknown>(null);
  const [editName, setEditName] = useState('');
  const [editBody, setEditBody] = useState('');
  const [editExpires, setEditExpires] = useState('');
  const [msg, setMsg] = useState('');

  const load = () => {
    getJson<{ items: OpencodeProfile[] }>('/v1/opencode-profiles').then((r) => {
      setProfiles(r.items);
      setError(null);
    }).catch(setError);
    getJson<{ items: NodeView[] }>('/v1/nodes').then((r) => setNodes(r.items)).catch(() => undefined);
  };
  useEffect(() => {
    load();
    const t = setInterval(load, POLL_MS);
    return () => clearInterval(t);
  }, []);

  // The upsert formatter validates JSON locally before any server round
  // trip (`dry_run` shows the post-sanitisation shape when keys are
  // allowed on the allowlist).
  const [dryResult, setDryResult] = useState<{ hash: string; dropped: string[]; effective: string } | null>(null);

  const formatBody = () => {
    try {
      setEditBody(JSON.stringify(JSON.parse(editBody), null, 2));
      setMsg('');
    } catch (e) {
      setMsg(`format: invalid JSON — ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const dryRun = async () => {
    if (!editName.trim() || !editBody.trim()) {
      setMsg('name and config body required');
      return;
    }
    try {
      const cfg = JSON.parse(editBody);
      const body = JSON.stringify({
        config: cfg,
        expires_at: editExpires.trim() ? editExpires.trim() : null,
      });
      const r = await fetch(
        `/v1/opencode-profiles/${encodeURIComponent(editName.trim())}?dry_run=true`,
        {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          credentials: 'include',
          body,
        },
      );
      const data = await r.json();
      if (!r.ok) throw new Error(`dry-run failed: ${r.status}`);
      setDryResult({
        hash: data.would_set_hash as string,
        dropped: (data.dropped_keys as string[]) ?? [],
        effective: JSON.stringify(data.effective_config, null, 2),
      });
      const drops = (data.dropped_keys as string[]).length;
      setMsg(`dry-run ok — ${drops} keys stripped`);
    } catch (e) {
      setDryResult(null);
      setMsg(`error: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const upsertSave = async () => {
    if (!editName.trim() || !editBody.trim()) {
      setMsg('name and config body required');
      return;
    }
    try {
      const cfg = JSON.parse(editBody);
      const body = JSON.stringify({
        config: cfg,
        expires_at: editExpires.trim() ? editExpires.trim() : null,
      });
      const r = await fetch(`/v1/opencode-profiles/${encodeURIComponent(editName.trim())}`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        credentials: 'include',
        body,
      });
      if (!r.ok) throw new Error(`upsert failed: ${r.status}`);
      setMsg('profile saved');
      setEditBody('');
      setEditName('');
      setEditExpires('');
      setDryResult(null);
      load();
    } catch (e) {
      setMsg(`error: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const preview = (p: OpencodeProfile) => ({
    hash: p.hash.slice(0, 16),
    keys: Object.keys(p.config).length,
    model: typeof p.config['model'] === 'string' ? p.config['model'] : null,
    small_model:
      typeof p.config['small_model'] === 'string' ? p.config['small_model'] : null,
    snapshot: p.config['snapshot'] === true,
    share: p.config['share'] === true,
  });

  const remove = async (name: string) => {
    const fallback = window.prompt(
      `Delete profile "${name}"?\n\nLeave empty to delete plain (assigned nodes keep their last-applied config).\nOr enter another profile name to move assigned nodes onto it first.`,
    );
    if (fallback === null) return;
    const q = fallback.trim() ? `?fallback=${encodeURIComponent(fallback.trim())}` : '';
    const r = await fetch(`/v1/opencode-profiles/${encodeURIComponent(name)}${q}`, {
      method: 'DELETE',
      credentials: 'include',
    });
    if (r.status === 204 || r.ok) load();
    else setMsg(`delete failed: ${r.status}`);
  };

  const assign = async (profileId: string, nodeId: string) => {
    try {
      await postJson(`/v1/nodes/${nodeId}/opencode-profile`, { profile_id: profileId });
      setMsg('assigned; node receives the push on the next ws notify');
      load();
    } catch (e) {
      setMsg(`assign failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const rollback = async (name: string) => {
    if (!window.confirm(`Roll back profile "${name}" to the previous revision?`)) return;
    const r = await fetch(`/v1/opencode-profiles/${encodeURIComponent(name)}/rollback`, {
      method: 'POST',
      credentials: 'include',
      headers: { 'content-type': 'application/json' },
      body: '{}',
    });
    if (r.ok) {
      setMsg('rolled back');
      load();
    } else if (r.status === 404) {
      setMsg('no prior revision kept — nothing to roll back to');
    } else {
      setMsg(`rollback failed: ${r.status}`);
    }
  };

  if (!profiles) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Opencode profiles</h2>
      {msg && <div className="muted">{msg}</div>}
      <div className="cards">
        {profiles.map((p) => {
          const v = preview(p);
          return (
            <div key={p.id} className="card">
              <div className="card-title">{p.name}</div>
              <div className="muted">hash {v.hash}… · {v.keys} keys</div>
              {v.model && <div className="muted">model {String(v.model)}</div>}
              {v.small_model && <div className="muted">small {String(v.small_model)}</div>}
              <div className="muted">snapshot {v.snapshot ? 'on' : 'off'} · share {v.share ? 'on' : 'off'}</div>
              <div className="muted">updated {fmtTime(p.updated_at)}</div>
              {p.expires_at && (
                <div className="muted">expires {fmtTime(p.expires_at)}</div>
              )}
              <details>
                <summary>config</summary>
                <pre>{JSON.stringify(p.config, null, 2)}</pre>
              </details>
              <div>
                <button onClick={() => remove(p.name)}>delete</button>
                {p.prev && (
                  <button onClick={() => rollback(p.name)} title="Swap to previous revision">
                    rollback
                  </button>
                )}
                <select
                  defaultValue=""
                  onChange={(e) => assign(p.id, e.target.value)}
                >
                  <option value="" disabled>assign to node…</option>
                  {nodes.map((n) => (
                    <option key={n.id} value={n.id}>
                      {n.name}{' '}
                      {n.opencode_profile_id === p.id ? ' ✓' : ''}
                    </option>
                  ))}
                </select>
              </div>
              {p.prev && (
                <details>
                  <summary>diff vs previous ({p.prev.hash.slice(0, 12)}…)</summary>
                  <ConfigDiff prev={p.prev.config} cur={p.config} />
                </details>
              )}
            </div>
          );
        })}
      </div>

      <h3>Upsert profile</h3>
      <div>
        <input
          placeholder="profile name"
          value={editName}
          onChange={(e) => setEditName(e.target.value)}
        />
        <textarea
          placeholder='{"model":"anthropic/claude-sonnet-4.5","small_model":"anthropic/claude-haiku"}'
          rows={10}
          value={editBody}
          onChange={(e) => setEditBody(e.target.value)}
          style={{ width: '100%', fontFamily: 'monospace', marginTop: 8 }}
        />
        <input
          placeholder="expires at (RFC3339, optional — e.g. 2026-01-01T00:00:00Z)"
          value={editExpires}
          onChange={(e) => setEditExpires(e.target.value)}
          style={{ width: '100%', marginTop: 8 }}
        />
        <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
          <button onClick={formatBody} disabled={!editBody.trim()} title="Pretty-print the JSON body">
            Format
          </button>
          <button onClick={dryRun}>Preview (dry-run)</button>
          <button onClick={upsertSave}>Save profile</button>
        </div>
        {dryResult && (
          <div className="card" style={{ marginTop: 8 }}>
            <div className="card-title">dry-run preview</div>
            <div className="muted">would-set hash {dryResult.hash.slice(0, 16)}…</div>
            {dryResult.dropped.length > 0 && (
              <div className="muted">
                dropped keys: {dryResult.dropped.join(', ')}
              </div>
            )}
            <pre>{dryResult.effective}</pre>
          </div>
        )}
      </div>
    </section>
  );
}
