import { useEffect, useState } from 'react';
import { ApiError, createTask, listNodes, listRepos, NodeView, RepositoryView } from '../api';
import { ErrorBox } from './util';

export default function NewTask({
  onCreated,
  onError,
}: {
  onCreated: (id: string) => void;
  onError: (e: unknown) => void;
}) {
  const [repository, setRepository] = useState('');
  const [prompt, setPrompt] = useState('');
  const [adapter, setAdapter] = useState('');
  const [validation, setValidation] = useState('');
  const [node, setNode] = useState('auto');
  const [timeout, setTimeout] = useState('');
  const [error, setError] = useState<Error | null>(null);
  const [busy, setBusy] = useState(false);
  const [repos, setRepos] = useState<RepositoryView[]>([]);
  const [nodes, setNodes] = useState<NodeView[]>([]);

  useEffect(() => {
    listRepos().then(setRepos).catch(() => {});
    listNodes().then(setNodes).catch(() => {});
  }, []);

  const adapterSuggestions = Array.from(
    new Set(nodes.flatMap((n) => n.adapters)),
  );

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!repository.trim() || !prompt.trim() || !adapter.trim()) {
      setError(new Error('Repository, prompt and adapter are required.'));
      return;
    }
    setBusy(true);
    try {
      const task = await createTask({
        repository: repository.trim(),
        prompt: prompt.trim(),
        adapter: adapter.trim(),
        validation_command: validation.trim() ? validation.trim() : undefined,
        requested_node_id: node === 'auto' ? undefined : node,
        timeout_secs: timeout.trim() ? Number(timeout) : undefined,
      });
      onCreated(task.id);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) onError(err);
      else setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="newtask">
      <h2>New task</h2>
      {error && <ErrorBox err={error} />}
      <form onSubmit={submit} className="form">
        <label>
          Repository
          <input
            list="repos"
            value={repository}
            onChange={(e) => setRepository(e.target.value)}
            placeholder="demo"
            required
          />
          <datalist id="repos">
            {repos.map((r) => (
              <option key={r.id} value={r.name} />
            ))}
          </datalist>
        </label>
        <label>
          Prompt
          <textarea
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            rows={5}
            placeholder="write:hello.txt:hello world"
            required
          />
        </label>
        <label>
          Adapter
          <input
            list="adapters"
            value={adapter}
            onChange={(e) => setAdapter(e.target.value)}
            placeholder="mock"
            required
          />
          <datalist id="adapters">
            {adapterSuggestions.map((a) => (
              <option key={a} value={a} />
            ))}
          </datalist>
        </label>
        <label>
          Validation command <span className="muted">(optional, overrides repo default)</span>
          <input
            value={validation}
            onChange={(e) => setValidation(e.target.value)}
            placeholder="cargo test"
          />
        </label>
        <label>
          Node
          <select value={node} onChange={(e) => setNode(e.target.value)}>
            <option value="auto">Auto (any eligible)</option>
            {nodes.map((n) => (
              <option key={n.id} value={n.id}>
                {n.name} ({n.status})
              </option>
            ))}
          </select>
        </label>
        <label>
          Timeout (seconds) <span className="muted">(optional, default 3600)</span>
          <input
            type="number"
            min={1}
            value={timeout}
            onChange={(e) => setTimeout(e.target.value)}
          />
        </label>
        <button type="submit" disabled={busy}>
          {busy ? 'Creating…' : 'Create task'}
        </button>
      </form>
    </section>
  );
}
