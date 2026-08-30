import { useEffect, useState } from "react";
import {
  ApiError,
  createTask,
  listNodes,
  listRepos,
  NodeView,
  RepositoryView,
} from "../api";
import { ErrorBox } from "./util";

export default function NewTask({
  onCreated,
  onError,
}: {
  onCreated: (id: string) => void;
  onError: (e: unknown) => void;
}) {
  const [repository, setRepository] = useState("");
  const [prompt, setPrompt] = useState("");
  const [adapter, setAdapter] = useState("");
  const [validation, setValidation] = useState("");
  const [node, setNode] = useState("auto");
  const [timeout, setTimeout] = useState("");
  const [error, setError] = useState<Error | null>(null);
  const [busy, setBusy] = useState(false);
  const [repos, setRepos] = useState<RepositoryView[]>([]);
  const [nodes, setNodes] = useState<NodeView[]>([]);

  // Advanced (optional) fields — all map 1:1 onto CreateTaskRequest.
  const [baseCommit, setBaseCommit] = useState("");
  const [networkMode, setNetworkMode] = useState("");
  const [securityProfile, setSecurityProfile] = useState("");
  const [maxAttempts, setMaxAttempts] = useState("");
  const [parentSessionId, setParentSessionId] = useState("");
  const [groupId, setGroupId] = useState("");
  const [agentId, setAgentId] = useState("");
  const [githubPush, setGithubPush] = useState(false);
  const [githubRepo, setGithubRepo] = useState("");
  const [githubIssue, setGithubIssue] = useState("");
  const [githubBaseRef, setGithubBaseRef] = useState("");
  // Consensus: N adapters vote on one prompt — one task per adapter,
  // all stamped with one group id (CP collapses the group when all land).
  const [consensusN, setConsensusN] = useState("");
  const [consensusModels, setConsensusModels] = useState("");
  // Per-task opencode model override (merged over the node's active profile).
  const [opencodeModel, setOpencodeModel] = useState("");
  const [opencodeSmallModel, setOpencodeSmallModel] = useState("");

  useEffect(() => {
    listRepos()
      .then(setRepos)
      .catch(() => {});
    listNodes()
      .then(setNodes)
      .catch(() => {});
  }, []);

  const adapterSuggestions = Array.from(
    new Set(nodes.flatMap((n) => n.adapters)),
  );

  const buildBody = (overrides: Record<string, unknown> = {}) => ({
    repository: repository.trim(),
    prompt: prompt.trim(),
    adapter: adapter.trim(),
    validation_command: validation.trim() || undefined,
    requested_node_id: node === "auto" ? undefined : node,
    timeout_secs: timeout.trim() ? Number(timeout) : undefined,
    base_commit: baseCommit.trim() || undefined,
    network_mode: networkMode || undefined,
    security_profile: securityProfile.trim() || undefined,
    max_attempts: maxAttempts.trim() ? Number(maxAttempts) : 1,
    parent_acp_session_id: parentSessionId.trim() || undefined,
    group_id: groupId.trim() || undefined,
    agent_id: agentId.trim() || undefined,
    github_push: githubPush,
    github_repo: githubRepo.trim() || undefined,
    github_issue: githubIssue.trim() ? Number(githubIssue) : undefined,
    github_base_ref: githubBaseRef.trim() || undefined,
    ...(opencodeModel.trim() || opencodeSmallModel.trim()
      ? {
          opencode_override: {
            model: opencodeModel.trim() || undefined,
            small_model: opencodeSmallModel.trim() || undefined,
          },
        }
      : {}),
    ...overrides,
  });

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!repository.trim() || !prompt.trim() || !adapter.trim()) {
      setError(new Error("Repository, prompt and adapter are required."));
      return;
    }
    setBusy(true);
    try {
      if (consensusN.trim()) {
        // Fan out N tasks, one per model — same shape as `ag run --consensus`.
        const n = Number(consensusN);
        const models = consensusModels
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean);
        if (!Number.isInteger(n) || n < 2 || models.length !== n) {
          throw new Error(
            `Consensus needs N ≥ 2 and exactly N models (N=${n}, got ${models.length}).`,
          );
        }
        const group = crypto.randomUUID();
        let lastId: string | null = null;
        for (const member of models) {
          const task = await createTask(
            buildBody({
              adapter: member,
              consensus_group_id: group,
              consensus_member: member,
              requested_node_id: undefined,
            }),
          );
          lastId = task.id;
        }
        onCreated(lastId!);
      } else {
        const task = await createTask(buildBody());
        onCreated(task.id);
      }
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
          Validation command{" "}
          <span className="muted">(optional, overrides repo default)</span>
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
          Timeout (seconds){" "}
          <span className="muted">(optional, default 3600)</span>
          <input
            type="number"
            min={1}
            value={timeout}
            onChange={(e) => setTimeout(e.target.value)}
          />
        </label>
        <details className="advanced">
          <summary>Advanced</summary>
          <label>
            Base commit <span className="muted">(optional)</span>
            <input
              value={baseCommit}
              onChange={(e) => setBaseCommit(e.target.value)}
              placeholder="abc123…"
            />
          </label>
          <label>
            Network mode <span className="muted">(node policy caps it)</span>
            <select
              value={networkMode}
              onChange={(e) => setNetworkMode(e.target.value)}
            >
              <option value="">default (node policy)</option>
              <option value="none">none</option>
              <option value="restricted">restricted</option>
              <option value="unrestricted">unrestricted</option>
            </select>
          </label>
          <label>
            Security profile <span className="muted">(e.g. strict)</span>
            <input
              value={securityProfile}
              onChange={(e) => setSecurityProfile(e.target.value)}
              placeholder="default"
            />
          </label>
          <label>
            Max attempts <span className="muted">(auto-retry on failure)</span>
            <input
              type="number"
              min={1}
              value={maxAttempts}
              onChange={(e) => setMaxAttempts(e.target.value)}
              placeholder="1"
            />
          </label>
          <label>
            ACP session to resume <span className="muted">(optional)</span>
            <input
              value={parentSessionId}
              onChange={(e) => setParentSessionId(e.target.value)}
            />
          </label>
          <label>
            Task group (shared context){" "}
            <span className="muted">(optional)</span>
            <input
              value={groupId}
              onChange={(e) => setGroupId(e.target.value)}
              placeholder="group-1"
            />
          </label>
          <label>
            Agent id <span className="muted">(budget-attributed task)</span>
            <input
              value={agentId}
              onChange={(e) => setAgentId(e.target.value)}
              placeholder="agent-abc"
            />
          </label>
          <label>
            Opencode model override <span className="muted">(optional)</span>
            <input
              value={opencodeModel}
              onChange={(e) => setOpencodeModel(e.target.value)}
              placeholder="claude-sonnet-4-5"
            />
          </label>
          <label>
            Opencode small-model override{" "}
            <span className="muted">(optional)</span>
            <input
              value={opencodeSmallModel}
              onChange={(e) => setOpencodeSmallModel(e.target.value)}
            />
          </label>
          <label className="row">
            <input
              type="checkbox"
              checked={githubPush}
              onChange={(e) => setGithubPush(e.target.checked)}
            />
            GitHub write-back (push branch + open PR after success)
          </label>
          {githubPush && (
            <>
              <label>
                GitHub repo <span className="muted">(owner/name)</span>
                <input
                  value={githubRepo}
                  onChange={(e) => setGithubRepo(e.target.value)}
                  placeholder="you/your-repo"
                />
              </label>
              <label>
                GitHub issue #{" "}
                <span className="muted">(commented on success, optional)</span>
                <input
                  type="number"
                  min={1}
                  value={githubIssue}
                  onChange={(e) => setGithubIssue(e.target.value)}
                />
              </label>
              <label>
                GitHub base ref{" "}
                <span className="muted">(defaults to repo default branch)</span>
                <input
                  value={githubBaseRef}
                  onChange={(e) => setGithubBaseRef(e.target.value)}
                  placeholder="main"
                />
              </label>
            </>
          )}
          <fieldset>
            <legend>Consensus (multi-adapter vote)</legend>
            <small className="muted">
              Fires N tasks, one per adapter, in one group; the CP collapses the
              group when all land. Disables per-task node pinning.
            </small>
            <label>
              N (≥ 2)
              <input
                type="number"
                min={2}
                value={consensusN}
                onChange={(e) => setConsensusN(e.target.value)}
                placeholder="2"
              />
            </label>
            <label>
              Models (comma-separated, must equal N)
              <input
                value={consensusModels}
                onChange={(e) => setConsensusModels(e.target.value)}
                placeholder="claude,opencode"
              />
            </label>
          </fieldset>
        </details>
        <details className="req-preview">
          <summary>Request preview</summary>
          <pre>{JSON.stringify(buildBody(), null, 2)}</pre>
        </details>
        <button type="submit" disabled={busy}>
          {busy ? "Creating…" : "Create task"}
        </button>
      </form>
    </section>
  );
}
