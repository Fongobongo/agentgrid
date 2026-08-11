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
  created_at: string;
  updated_at: string;
}

interface NodeView {
  id: string;
  name: string;
  opencode_profile_id: string | null;
}

const POLL_MS = 5000;

export default function OpencodeProfiles() {
  const [profiles, setProfiles] = useState<OpencodeProfile[] | null>(null);
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [error, setError] = useState<unknown>(null);
  const [editName, setEditName] = useState('');
  const [editBody, setEditBody] = useState('');
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

  const upsert = async () => {
    if (!editName.trim() || !editBody.trim()) {
      setMsg('name and config body required');
      return;
    }
    try {
      const cfg = JSON.parse(editBody);
      const r = await fetch(`/v1/opencode-profiles/${encodeURIComponent(editName.trim())}`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        credentials: 'include',
        body: JSON.stringify({ config: cfg }),
      });
      if (!r.ok) throw new Error(`upsert failed: ${r.status}`);
      setMsg('profile saved');
      setEditBody('');
      setEditName('');
      load();
    } catch (e) {
      setMsg(`error: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const remove = async (name: string) => {
    if (!window.confirm(`Delete profile "${name}"?`)) return;
    const r = await fetch(`/v1/opencode-profiles/${encodeURIComponent(name)}`, {
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

  if (!profiles) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Opencode profiles</h2>
      {msg && <div className="muted">{msg}</div>}
      <div className="cards">
        {profiles.map((p) => (
          <div key={p.id} className="card">
            <div className="card-title">{p.name}</div>
            <div className="muted">hash {p.hash.slice(0, 16)}…</div>
            <div className="muted">updated {fmtTime(p.updated_at)}</div>
            <details>
              <summary>config</summary>
              <pre>{JSON.stringify(p.config, null, 2)}</pre>
            </details>
            <div>
              <button onClick={() => remove(p.name)}>delete</button>
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
          </div>
        ))}
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
        <button onClick={upsert}>Save profile</button>
      </div>
    </section>
  );
}
