import { useEffect, useState } from "react";
import { listProxies, addProxy, removeProxy, ProxyView } from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

// Egress proxy pool (ADR 0012): global pool rows first, then node-scoped.
// Nodes pick the list up on every poll and rotate on failure.
export default function Proxies() {
  const [items, setItems] = useState<ProxyView[] | null>(null);
  const [url, setUrl] = useState("");
  const [node, setNode] = useState("");
  const [error, setError] = useState<Error | null>(null);

  const refresh = () =>
    listProxies()
      .then(setItems)
      .catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!url.trim()) return;
    try {
      await addProxy(url.trim(), node.trim() || null);
      setUrl("");
      setNode("");
      refresh();
    } catch (err) {
      setError(err as Error);
    }
  };

  const remove = async (id: number) => {
    await removeProxy(id).catch(setError);
    refresh();
  };

  if (!items) return <Loading />;
  return (
    <section>
      <h2>Egress proxies</h2>
      <p className="hint">
        Global pool applies to every node; node-scoped entries append after
        it. A proxy that fails connect/timeout is quarantined 5 min, then
        rejoined. Node env <code>AGENTGRID_PROXY_URLS</code> overrides the
        pushed list.
      </p>
      <form onSubmit={submit} className="rowform">
        <input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="http://user:pass@host:port or socks5://host:port"
          required
        />
        <input
          value={node}
          onChange={(e) => setNode(e.target.value)}
          placeholder="node id (empty = global)"
        />
        <button type="submit">Add proxy</button>
      </form>
      {error && <ErrorBox err={error} />}
      <table>
        <thead>
          <tr>
            <th>ID</th>
            <th>URL</th>
            <th>Scope</th>
            <th>Added</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {items.map((p) => (
            <tr key={p.id}>
              <td>#{p.id}</td>
              <td>
                <code>{p.url}</code>
              </td>
              <td>{p.node_id ?? "global"}</td>
              <td>{fmtTime(p.created_at)}</td>
              <td>
                <button onClick={() => remove(p.id)}>Remove</button>
              </td>
            </tr>
          ))}
          {items.length === 0 && (
            <tr>
              <td colSpan={5}>No proxies — nodes use direct egress.</td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}
