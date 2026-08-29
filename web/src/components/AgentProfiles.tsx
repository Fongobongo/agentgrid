import { useEffect, useState } from "react";
import {
  AgentProfileRevision,
  activateProfileRevision,
  createProfileRevision,
  listAgentProfiles,
  listProfileRevisions,
} from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

export default function AgentProfiles() {
  const [profiles, setProfiles] = useState<string[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [revs, setRevs] = useState<AgentProfileRevision[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [id, setId] = useState("");
  const [prompt, setPrompt] = useState("");
  const [autonomy, setAutonomy] = useState("l2");
  const [memMax, setMemMax] = useState("");
  const [cpuQuota, setCpuQuota] = useState("");
  const [tasksMax, setTasksMax] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = () => listAgentProfiles().then(setProfiles).catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  useEffect(() => {
    setRevs(null);
    if (selected)
      listProfileRevisions(selected)
        .then(setRevs)
        .catch(() => setRevs([]));
  }, [selected]);

  const createRev = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await createProfileRevision(id.trim(), {
        system_prompt: prompt,
        autonomy,
        memory_max: memMax.trim() ? Number(memMax) : null,
        cpu_quota: cpuQuota.trim() ? Number(cpuQuota) : null,
        tasks_max: tasksMax.trim() ? Number(tasksMax) : null,
      });
      setId("");
      setPrompt("");
      setMemMax("");
      setCpuQuota("");
      setTasksMax("");
      await refresh();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Agent profiles</h2>
      <p className="muted">
        Stage 13: system prompt + autonomy + resource limits per profile id.
        Revisions are immutable; activating a revision points the node at it.
      </p>
      {error && <ErrorBox err={error} />}
      {profiles === null ? (
        <Loading />
      ) : (
        <ul
          style={{
            display: "flex",
            gap: 8,
            flexWrap: "wrap",
            listStyle: "none",
            padding: 0,
          }}
        >
          {profiles.map((p) => (
            <li key={p}>
              <button
                onClick={() => setSelected(p)}
                className={selected === p ? "active" : ""}
              >
                {p}
              </button>
            </li>
          ))}
        </ul>
      )}
      {selected && (
        <section style={{ marginTop: 12 }}>
          <h3>{selected} — revisions</h3>
          {revs === null ? (
            <Loading />
          ) : (
            <table>
              <thead>
                <tr>
                  <th>Rev</th>
                  <th>Autonomy</th>
                  <th>Limits (MiB/%/tasks)</th>
                  <th>Prompt</th>
                  <th>By</th>
                  <th>Created</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {revs.map((r) => (
                  <tr
                    key={r.revision}
                    style={{ fontWeight: r.active ? 600 : 400 }}
                  >
                    <td>
                      {r.revision}
                      {r.active ? " (active)" : ""}
                    </td>
                    <td>{r.autonomy}</td>
                    <td>
                      {r.memory_max !== null
                        ? `${Math.round(r.memory_max / (1024 * 1024))}MiB`
                        : "—"}{" "}
                      / {r.cpu_quota !== null ? `${r.cpu_quota}%` : "—"} /{" "}
                      {r.tasks_max !== null ? r.tasks_max : "—"}
                    </td>
                    <td style={{ maxWidth: 300 }}>
                      <code>
                        {r.system_prompt.slice(0, 80) || "—"}
                        {r.system_prompt.length > 80 ? "…" : ""}
                      </code>
                    </td>
                    <td>{r.created_by ?? "—"}</td>
                    <td>{fmtTime(r.created_at)}</td>
                    <td>
                      {!r.active && (
                        <button
                          onClick={() =>
                            activateProfileRevision(selected, r.revision)
                              .then(() =>
                                listProfileRevisions(selected).then(setRevs),
                              )
                              .catch(setError)
                          }
                        >
                          Activate
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      )}
      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>Create revision</summary>
        <form onSubmit={createRev} className="form">
          <label>
            Profile id
            <input
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="claude-main"
              required
            />
          </label>
          <label>
            Autonomy
            <select
              value={autonomy}
              onChange={(e) => setAutonomy(e.target.value)}
            >
              {["l0", "l1", "l2", "l3", "l4"].map((l) => (
                <option key={l}>{l}</option>
              ))}
            </select>
          </label>
          <label>
            System prompt
            <textarea
              rows={4}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
            />
          </label>
          <label>
            Memory max (bytes)
            <input
              type="number"
              value={memMax}
              onChange={(e) => setMemMax(e.target.value)}
              placeholder="268435456"
            />
          </label>
          <label>
            CPU quota (%)
            <input
              type="number"
              value={cpuQuota}
              onChange={(e) => setCpuQuota(e.target.value)}
              placeholder="200"
            />
          </label>
          <label>
            Tasks max
            <input
              type="number"
              value={tasksMax}
              onChange={(e) => setTasksMax(e.target.value)}
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create revision"}
          </button>
        </form>
      </details>
    </section>
  );
}
