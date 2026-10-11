import { useState } from "react";
import { ApiError, createAssembly } from "../api";
import { ErrorBox } from "./util";

export interface AssemblyForm {
  goal: string;
  adapter: string;
  repository: string;
  autoApprove: boolean;
  githubPush: boolean;
  githubRepo: string;
  githubIssue: string;
  githubCreate: boolean;
}

/// Pure body builder for `POST /v1/assembly` (unit-tested; the component
/// only wires inputs). Empty optionals are dropped so the server defaults
/// apply (mock adapter, manual approval, no write-back).
export function buildAssemblyBody(f: AssemblyForm): Record<string, unknown> {
  const body: Record<string, unknown> = { goal: f.goal.trim() };
  if (f.repository.trim()) body.repository = f.repository.trim();
  if (f.adapter.trim()) body.adapter = f.adapter.trim();
  if (f.autoApprove) body.auto_approve_plan = true;
  if (f.githubPush) body.github_push = true;
  if (f.githubRepo.trim()) body.github_repo = f.githubRepo.trim();
  if (f.githubIssue.trim()) body.github_issue = Number(f.githubIssue);
  if (f.githubCreate) body.github_create = true;
  return body;
}

export default function Assembly({
  onCreated,
  onError,
}: {
  onCreated: (runId: string) => void;
  onError: (e: unknown) => void;
}) {
  const [goal, setGoal] = useState("");
  const [adapter, setAdapter] = useState("");
  const [repository, setRepository] = useState("");
  const [autoApprove, setAutoApprove] = useState(false);
  const [githubPush, setGithubPush] = useState(false);
  const [githubRepo, setGithubRepo] = useState("");
  const [githubIssue, setGithubIssue] = useState("");
  const [githubCreate, setGithubCreate] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!goal.trim()) {
      setError(new Error("Goal is required."));
      return;
    }
    if (githubIssue.trim() && !/^\d+$/.test(githubIssue.trim())) {
      setError(new Error("GitHub issue must be a positive number."));
      return;
    }
    setBusy(true);
    try {
      const run = await createAssembly(
        buildAssemblyBody({
          goal,
          adapter,
          repository,
          autoApprove,
          githubPush,
          githubRepo,
          githubIssue,
          githubCreate,
        }),
      );
      onCreated(run.id);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) onError(err);
      else setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="newtask">
      <h2>Assemble project</h2>
      <p>
        Goal in, architect-planned multi-host build out: the server drafts an
        architect step, expands the plan (auto-approved only if checked),
        fans workers out, merges via a collector, and optionally publishes to
        GitHub.
      </p>
      {error && <ErrorBox err={error} />}
      <form onSubmit={submit} className="form">
        <label>
          Goal
          <textarea
            value={goal}
            onChange={(e) => setGoal(e.target.value)}
            rows={4}
            placeholder="Build a static blog with an RSS feed"
          />
        </label>
        <label>
          Adapter (architect + workers; empty = mock)
          <input
            value={adapter}
            onChange={(e) => setAdapter(e.target.value)}
            placeholder="claude"
          />
        </label>
        <label>
          Repository (empty = plain-dir assembly)
          <input
            value={repository}
            onChange={(e) => setRepository(e.target.value)}
            placeholder="myrepo"
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={autoApprove}
            onChange={(e) => setAutoApprove(e.target.checked)}
          />
          Auto-approve plan (skip the PlanReady pause; audited)
        </label>
        <details>
          <summary>GitHub publish (collector step)</summary>
          <label className="check">
            <input
              type="checkbox"
              checked={githubPush}
              onChange={(e) => setGithubPush(e.target.checked)}
            />
            Push assembled project
          </label>
          <label>
            Repository (owner/name)
            <input
              value={githubRepo}
              onChange={(e) => setGithubRepo(e.target.value)}
              placeholder="acme/blog"
            />
          </label>
          <label>
            Issue number to report on (optional)
            <input
              value={githubIssue}
              onChange={(e) => setGithubIssue(e.target.value)}
              placeholder="7"
            />
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={githubCreate}
              onChange={(e) => setGithubCreate(e.target.checked)}
            />
            Create remote if missing (private)
          </label>
        </details>
        <button type="submit" disabled={busy}>
          {busy ? "Starting…" : "Assemble"}
        </button>
      </form>
      <pre>{JSON.stringify(buildAssemblyBody({ goal, adapter, repository, autoApprove, githubPush, githubRepo, githubIssue, githubCreate }), null, 2)}</pre>
    </section>
  );
}
