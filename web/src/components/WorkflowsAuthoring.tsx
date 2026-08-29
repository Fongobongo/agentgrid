import { useEffect, useState } from "react";
import { listGet, postJson } from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

interface WorkflowTemplate {
  id: string;
  name: string;
  steps: {
    id: string;
    prompt: string;
    adapter?: string;
    depends_on?: string[];
  }[];
  created_at: string;
}

interface WorkflowSchedule {
  id: string;
  template_id: string;
  interval_seconds: number;
  autonomy: string;
  last_run_at?: string;
  enabled: boolean;
}

export default function WorkflowsAuthoring({
  onCreated,
}: {
  onCreated: (id: string) => void;
}) {
  const [templates, setTemplates] = useState<WorkflowTemplate[] | null>(null);
  const [schedules, setSchedules] = useState<WorkflowSchedule[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [name, setName] = useState("");
  const [stepsYaml, setStepsYaml] = useState(
    '# one step per line:\n# - id: review\n#   prompt: "review the diff"\n#   adapter: mock\n# - id: verify\n#   prompt: "validate"\n#   depends_on: [review]\n',
  );
  const [repo, setRepo] = useState("");
  const [budgetMaxUsd, setBudgetMaxUsd] = useState("");
  const [busy, setBusy] = useState(false);
  const [schedTemplateId, setSchedTemplateId] = useState("");
  const [schedIntervalSecs, setSchedIntervalSecs] = useState("");
  const [schedAutonomy, setSchedAutonomy] = useState("l2");

  const load = async () => {
    try {
      const t = await listGet<WorkflowTemplate>("/v1/workflows");
      setTemplates(t);
      if (t[0] && !schedTemplateId) {
        setSchedTemplateId(t[0].id);
        loadSchedules(t[0].id);
      }
    } catch (e) {
      setError(e as Error);
    }
  };

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const loadSchedules = async (templateId: string) => {
    if (!templateId) return;
    try {
      // schedules are scoped to a template — there is no global list.
      const s = await listGet<WorkflowSchedule>(
        `/v1/workflows/${templateId}/schedules`,
      );
      setSchedules(s);
      setSchedTemplateId(templateId);
    } catch (e) {
      setError(e as Error);
    }
  };

  const createTemplate = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const tmpl = await postJson<WorkflowTemplate>("/v1/workflows", {
        name: name.trim(),
        steps: parseSteps(stepsYaml),
        budget: budgetMaxUsd.trim()
          ? { max_cost_cents: Math.round(Number(budgetMaxUsd) * 100) }
          : null,
      });
      setName("");
      setStepsYaml("");
      await load();
      if (repo.trim()) {
        const run = await postJson<{ id: string }>(
          `/v1/workflows/${tmpl.id}/runs`,
          {
            repository: repo.trim(),
          },
        );
        onCreated(run.id);
      }
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  const createSchedule = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await postJson(`/v1/workflows/${schedTemplateId}/schedules`, {
        interval_seconds: Number(schedIntervalSecs),
        autonomy: schedAutonomy,
        enabled: true,
      });
      setSchedIntervalSecs("");
      await loadSchedules(schedTemplateId);
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Workflow authoring</h2>
      <p className="muted">
        DAG of steps, budget-aware. Use <code>ag workflow</code> for full YAML
        editing; this panel covers the common create + schedule happy path.
      </p>
      {error && <ErrorBox err={error} />}

      {templates === null ? (
        <Loading />
      ) : (
        <>
          <h3>Templates</h3>
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Steps</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {templates.map((t) => (
                <tr key={t.id}>
                  <td>{t.name}</td>
                  <td>{t.steps.length}</td>
                  <td>{fmtTime(t.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}

      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>New template</summary>
        <form onSubmit={createTemplate} className="form">
          <label>
            Name
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="nightly-review"
              required
            />
          </label>
          <label>
            Steps (YAML list; <code>- id, prompt, depends_on?, adapter?</code>)
            <textarea
              rows={8}
              value={stepsYaml}
              onChange={(e) => setStepsYaml(e.target.value)}
              required
            />
          </label>
          <label>
            Repository to fire a run against{" "}
            <span className="muted">
              (optional — omit to only save the template)
            </span>
            <input
              value={repo}
              onChange={(e) => setRepo(e.target.value)}
              placeholder="demo"
            />
          </label>
          <label>
            Budget cap (USD){" "}
            <span className="muted">
              (optional circuit breaker, costs counted in cents)
            </span>
            <input
              type="number"
              step="0.01"
              value={budgetMaxUsd}
              onChange={(e) => setBudgetMaxUsd(e.target.value)}
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create template"}
          </button>
        </form>
      </details>

      <details className="advanced" style={{ marginTop: 12 }}>
        <summary>Schedules (per template)</summary>
        <p style={{ marginBottom: 8 }}>
          <label style={{ display: "inline-block", marginRight: 8 }}>
            Template:
          </label>
          <select
            value={schedTemplateId}
            onChange={(e) => {
              setSchedTemplateId(e.target.value);
              loadSchedules(e.target.value);
            }}
          >
            {templates?.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </p>
        {schedules === null ? (
          <p className="muted">Select a template…</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Interval (s)</th>
                <th>Autonomy</th>
                <th>Last run</th>
                <th>Enabled</th>
              </tr>
            </thead>
            <tbody>
              {schedules.map((s) => (
                <tr key={s.id}>
                  <td>{s.interval_seconds}</td>
                  <td>{s.autonomy}</td>
                  <td>{s.last_run_at ? fmtTime(s.last_run_at) : "—"}</td>
                  <td>{s.enabled ? "✓" : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <form
          onSubmit={createSchedule}
          className="form"
          style={{ marginTop: 12 }}
        >
          <label>
            Interval (seconds)
            <input
              type="number"
              min={1}
              value={schedIntervalSecs}
              onChange={(e) => setSchedIntervalSecs(e.target.value)}
              placeholder="86400"
              required
            />
          </label>
          <label>
            Autonomy
            <select
              value={schedAutonomy}
              onChange={(e) => setSchedAutonomy(e.target.value)}
            >
              {["l0", "l1", "l2", "l3", "l4"].map((l) => (
                <option key={l}>{l}</option>
              ))}
            </select>
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Add schedule"}
          </button>
        </form>
      </details>
    </section>
  );
}

// Lazy parser: accept a YAML template body when one is pasted wholesale
// (the server also accepts YAML), else parse a simple list-of-steps shape.
function parseSteps(
  text: string,
): { id: string; prompt: string; adapter?: string; depends_on?: string[] }[] {
  const parsed: {
    id: string;
    prompt: string;
    adapter?: string;
    depends_on?: string[];
  }[] = [];
  const lines = text.split("\n");
  let current: {
    id?: string;
    prompt?: string;
    adapter?: string;
    depends_on?: string[];
  } | null = null;
  const flush = () => {
    if (current && current.id && current.prompt) {
      parsed.push({
        id: current.id,
        prompt: current.prompt,
        adapter: current.adapter,
        depends_on: current.depends_on,
      });
    }
    current = null;
  };
  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const m = /^-\s*(.+)$/.exec(line);
    if (m) {
      flush();
      // `- id: foo` or starting a new step on its own line.
      const rest = m[1];
      const colon = rest.indexOf(":");
      if (colon > 0) {
        const k = rest.slice(0, colon).trim();
        const v = rest
          .slice(colon + 1)
          .trim()
          .replace(/^"(.*)"$/, "$1");
        current = { [k]: v } as typeof current;
      } else {
        current = {};
      }
      continue;
    }
    const m2 = /^([\w_]+):\s*(.*)$/.exec(line);
    if (current && m2) {
      const k = m2[1];
      const v = m2[2].trim().replace(/^"(.*)"$/, "$1");
      if (k === "depends_on") {
        current.depends_on = v
          .replace(/\[|\]/g, "")
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean);
      } else if (k === "adapter" || k === "prompt" || k === "id") {
        (current as Record<string, string>)[k] = v;
      }
    }
  }
  flush();
  return parsed;
}
