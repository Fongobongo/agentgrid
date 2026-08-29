import { useEffect, useState } from "react";
import {
  Agent,
  AgentAction,
  createAgent,
  listAgentActions,
  listAgents,
} from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

export default function Agents() {
  const [agents, setAgents] = useState<Agent[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [selected, setSelected] = useState<Agent | null>(null);
  const [actions, setActions] = useState<AgentAction[] | null>(null);
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [budget, setBudget] = useState("");
  const [maxTasks, setMaxTasks] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = () => listAgents().then(setAgents).catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  useEffect(() => {
    setActions(null);
    if (selected)
      listAgentActions(selected.id)
        .then(setActions)
        .catch(() => setActions([]));
  }, [selected]);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await createAgent({
        name: name.trim(),
        role: role.trim() || undefined,
        budget_usd: budget.trim() ? Number(budget) : undefined,
        max_tasks: maxTasks.trim() ? Number(maxTasks) : undefined,
      });
      setName("");
      setRole("");
      setBudget("");
      setMaxTasks("");
      await refresh();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Agents</h2>
      <p className="muted">
        Org agents with budget bookkeeping — tasks attributed via `agent_id` on
        create.
      </p>
      {error && <ErrorBox err={error} />}
      {agents === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Role</th>
              <th>Tasks spent</th>
              <th>Max tasks</th>
              <th>Created</th>
            </tr>
          </thead>
          <tbody>
            {agents.map((a) => (
              <tr
                key={a.id}
                onClick={() => setSelected(a)}
                style={{
                  cursor: "pointer",
                  background: selected?.id === a.id ? "var(--bg2)" : undefined,
                }}
              >
                <td>{a.name}</td>
                <td>{a.role}</td>
                <td>{a.tasks_spent}</td>
                <td>{a.max_tasks ?? "∞"}</td>
                <td>{fmtTime(a.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {selected && (
        <section style={{ marginTop: 16 }}>
          <h3>Actions — {selected.name}</h3>
          {actions === null ? (
            <Loading />
          ) : actions.length === 0 ? (
            <p className="muted">No recorded actions.</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>Action</th>
                  <th>Detail</th>
                  <th>At</th>
                </tr>
              </thead>
              <tbody>
                {actions.map((x) => (
                  <tr key={x.id}>
                    <td>{x.action}</td>
                    <td>{x.detail || "—"}</td>
                    <td>{fmtTime(x.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      )}
      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>Register agent</summary>
        <form onSubmit={submit} className="form">
          <label>
            Name
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
            />
          </label>
          <label>
            Role
            <input
              value={role}
              onChange={(e) => setRole(e.target.value)}
              placeholder="coder / reviewer"
            />
          </label>
          <label>
            Budget USD
            <input
              type="number"
              step="0.01"
              min={0}
              value={budget}
              onChange={(e) => setBudget(e.target.value)}
              placeholder="0 = unlimited"
            />
          </label>
          <label>
            Max tasks
            <input
              type="number"
              min={1}
              value={maxTasks}
              onChange={(e) => setMaxTasks(e.target.value)}
              placeholder="∞"
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create agent"}
          </button>
        </form>
      </details>
    </section>
  );
}
