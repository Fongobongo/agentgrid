import { useEffect, useState } from "react";
import {
  createMcpServer,
  deleteMcpServer,
  listMcpServers,
  McpServer,
} from "../api";
import { ErrorBox, Loading } from "./util";

export default function McpServers() {
  const [servers, setServers] = useState<McpServer[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [envReqs, setEnvReqs] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = () => listMcpServers().then(setServers).catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      // Server-side scanner rejects Critical findings; we surface the reason.
      await createMcpServer({
        id: id.trim(),
        name: name.trim(),
        command: command.trim(),
        args: args.trim() ? args.trim().split(/\s+/) : [],
        env_requirements: envReqs.trim()
          ? envReqs
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean)
          : [],
        enabled: true,
      });
      setId("");
      setName("");
      setCommand("");
      setArgs("");
      setEnvReqs("");
      await refresh();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>MCP servers</h2>
      <p className="muted">
        Registry of stdio MCP servers a profile attaches to. Registration is
        scanned server-side; a Critical pattern is rejected with the finding
        list.
      </p>
      {error && <ErrorBox err={error} />}
      {servers === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>ID</th>
              <th>Name</th>
              <th>Command</th>
              <th>Args</th>
              <th>Env req</th>
              <th>Enabled</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {servers.map((s) => (
              <tr key={s.id}>
                <td>{s.id}</td>
                <td>{s.name}</td>
                <td>
                  <code>{s.command}</code>
                </td>
                <td>
                  <code>{s.args.join(" ")}</code>
                </td>
                <td>{s.env_requirements.join(", ") || "—"}</td>
                <td>{s.enabled ? "✓" : "—"}</td>
                <td>
                  <button
                    className="danger"
                    onClick={() =>
                      deleteMcpServer(s.id).then(refresh).catch(setError)
                    }
                  >
                    Delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>Register MCP server</summary>
        <form onSubmit={submit} className="form">
          <label>
            ID
            <input
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="fs-tools"
              required
            />
          </label>
          <label>
            Name
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Filesystem tools"
              required
            />
          </label>
          <label>
            Command
            <input
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="mcp-server-fs"
              required
            />
          </label>
          <label>
            Args (space-separated)
            <input
              value={args}
              onChange={(e) => setArgs(e.target.value)}
              placeholder="--read-only"
            />
          </label>
          <label>
            Env requirements (comma-separated)
            <input
              value={envReqs}
              onChange={(e) => setEnvReqs(e.target.value)}
              placeholder="HOME"
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Registering…" : "Register"}
          </button>
        </form>
      </details>
    </section>
  );
}
