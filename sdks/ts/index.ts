// Agentgrid TypeScript SDK (plan 1.11 / roadmap #8).
//
// Thin client over the /v1 HTTP API — no dependencies, Node >= 18 (fetch).
// Auth: pass a JWT token (from `ag login` or `POST /v1/auth/login`) or set
// AGENTGRID_TOKEN. Usage:
//
//   import { Agentgrid } from "@agentgrid/sdk";
//   const ag = new Agentgrid("http://127.0.0.1:7800", process.env.AGENTGRID_TOKEN!);
//   const task = await ag.run("fix the flaky test", "my-org/my-repo", { adapter: "opencode" });
//   await ag.wait(task.id);                 // poll until terminal
//   const arts = await ag.artifacts(task.id);
//   console.log(await ag.artifact(task.id, arts[0].name));
//
// Surface: run | wait | cancel | artifacts | artifact | status.
//
// `run()` takes every field of `CreateTaskRequest` (docs/openapi.yaml), not a
// hand-picked subset: a thin client that silently drops `max_attempts` or
// `retry_on_node_loss` cannot express what the API can. Unset (`undefined`)
// fields are *omitted* from the body so the server's own default applies —
// sending an explicit null for a non-Option field (`max_attempts`,
// `github_push`, `github_create`) is a 400, not a default.

export interface OpencodeOverride {
  model?: string;
  small_model?: string;
  /** Partial opencode config object; override keys win over the node profile. */
  config?: Record<string, unknown>;
}

export type TaskStatus =
  | "queued"
  | "assigned"
  | "running"
  | "validating"
  | "succeeded"
  | "failed"
  | "cancelled";

/**
 * A task as the control plane returns it (`TaskView`).
 *
 * Fields marked `?` are omitted from the JSON when unset (`skip_serializing_if`
 * on the Rust side), so absent and `null` both mean "unset". The rest are
 * always present, nullable ones as an explicit `null`.
 */
export interface Task {
  id: string;
  repository: string;
  prompt: string;
  adapter: string;
  status: TaskStatus;
  created_at: string;
  finished_at: string | null;
  assigned_attempt_id: string | null;
  validation_command: string | null;
  error_code: string | null;
  requested_node_id: string | null;
  base_commit: string | null;
  parent_acp_session_id: string | null;
  network_mode?: string | null;
  security_profile?: string | null;
  group_id?: string | null;
  agent_id?: string | null;
  consensus_group_id?: string | null;
  consensus_member?: string | null;
  consensus_mode?: string | null;
  review_of?: string | null;
  opencode_override?: OpencodeOverride | null;
  github_repo?: string | null;
  github_issue?: number | null;
  github_base_ref?: string | null;
  github_create: boolean;
  rework_of?: string | null;
  max_attempts: number;
  max_tokens?: number | null;
  max_cost_cents?: number | null;
}

/** Body of `POST /v1/tasks` (`CreateTaskRequest`), minus the two positional
 * arguments of `run()`. Only `adapter` is required here — `prompt` and
 * `repository` are the positional parameters. */
export interface CreateTaskInput {
  /** Adapter to run (`opencode`, `claude-code`, `mock`, ...). */
  adapter: string;
  requested_node_id?: string;
  timeout_secs?: number;
  validation_command?: string;
  base_commit?: string;
  parent_acp_session_id?: string;
  /** Consensus fan-out tags — normally stamped by the CP, accepted for parity. */
  consensus_group_id?: string;
  consensus_member?: string;
  consensus_mode?: "solve" | "review";
  review_of?: string;
  /** Per-task ceilings; the attempt is killed with `budget_exceeded` past them. */
  max_tokens?: number;
  max_cost_cents?: number;
  network_mode?: "none" | "restricted" | "unrestricted";
  security_profile?: string;
  group_id?: string;
  agent_id?: string;
  opencode_override?: OpencodeOverride;
  github_push?: boolean;
  github_repo?: string;
  github_issue?: number;
  github_base_ref?: string;
  github_create?: boolean;
  /** Total attempts allowed; 1 (server default) = no auto-retry. */
  max_attempts?: number;
  /** Whether losing the node may spend the `max_attempts` budget (default true). */
  retry_on_node_loss?: boolean;
}

export interface ArtifactMeta {
  name: string;
  size_bytes: number;
  media_type?: string | null;
  sha256?: string | null;
}

const TERMINAL: ReadonlySet<string> = new Set([
  "succeeded",
  "failed",
  "cancelled",
  "blocked",
]);

export class Agentgrid {
  private token: string;
  private bases: string[];
  constructor(
    base: string | string[],
    token?: string,
  ) {
    // `replace(/\/+$/, "")` backtracks quadratically on a long run of
    // trailing slashes (CodeQL js/polynomial-redos); trimming in a loop is
    // linear in the length of the input.
    this.bases = (Array.isArray(base) ? base : [base]).map((b) => {
      let s = b;
      while (s.endsWith("/")) s = s.slice(0, -1);
      return s;
    });
    const env: Record<string, string | undefined> =
      (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ??
      {};
    this.token = token ?? env.AGENTGRID_TOKEN ?? "";
  }

  private get base(): string {
    return this.bases[0];
  }

  /** Move a hinted/ordered replica to the front of the list. */
  private prefer(hint: string | null): void {
    if (!hint) return;
    const url = hint.replace(/\/+$/, "");
    const i = this.bases.indexOf(url);
    if (i > 0) {
      this.bases.splice(i, 1);
      this.bases.unshift(url);
    }
  }

  private async req(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<any> {
    const headers: Record<string, string> = {};
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;
    if (body !== undefined) headers["content-type"] = "application/json";
    let lastError: unknown = new Error(`agentgrid ${method} ${path}: no control-plane URL`);
    // Snapshot the order: following a leader hint reorders `bases` for later
    // calls, but this pass must still try each replica exactly once.
    const order = this.bases.slice();
    for (let i = 0; i < order.length; i++) {
      let resp: Response;
      try {
        resp = await fetch(`${order[i]}${path}`, {
          method,
          headers,
          body: body !== undefined ? JSON.stringify(body) : undefined,
        });
      } catch (e) {
        lastError = e;
        continue;
      }
      if (resp.status === 503 && i + 1 < order.length) {
        this.prefer(resp.headers.get("x-agentgrid-leader"));
        lastError = new Error(`agentgrid ${method} ${path} -> 503 (not leader)`);
        continue;
      }
      if (!resp.ok) {
        throw new Error(`agentgrid ${method} ${path} -> ${resp.status} ${await resp.text()}`);
      }
      if (resp.status === 204) return undefined;
      // A 200 with an empty body is a normal success here — `POST
      // /v1/tasks/{id}/cancel` answers `200` and no payload. `resp.json()`
      // throws "Unexpected end of JSON input" on that, which made `cancel()`
      // fail after it had already cancelled the task.
      const text = await resp.text();
      return text.length === 0 ? undefined : JSON.parse(text);
    }
    throw lastError;
  }

  /** Login and return the JWT (same as `ag login`). */
  async login(username: string, password: string): Promise<string> {
    const r = await this.req("post", "/v1/auth/login", { username, password });
    this.token = r.token;
    return r.token;
  }

  /** Run a task: `run(prompt, repository, opts) -> Task`. */
  async run(
    prompt: string,
    repository: string,
    opts: CreateTaskInput,
  ): Promise<Task> {
    const body: Record<string, unknown> = { prompt, repository };
    for (const [k, v] of Object.entries(opts)) {
      // `undefined` means "server default" — omit, never null (see header).
      if (v !== undefined) body[k] = v;
    }
    return this.req("post", "/v1/tasks", body);
  }

  /** Current task status. */
  async status(id: string): Promise<string> {
    const t: Task = await this.req("get", `/v1/tasks/${id}`);
    return t.status;
  }

  /**
   * Poll until the task reaches a terminal status.
   * @param intervalMs poll interval (default 2000)
   * @param timeoutMs give up after this wall time (default 5 minutes)
   */
  async wait(id: string, intervalMs = 2000, timeoutMs = 300_000): Promise<Task> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const t: Task = await this.req("get", `/v1/tasks/${id}`);
      if (TERMINAL.has(t.status)) return t;
      if (Date.now() > deadline) throw new Error(`agentgrid wait(${id}) timed out; last status ${t.status}`);
      await new Promise((r) => setTimeout(r, intervalMs));
    }
  }

  /** Cancel a queued/running task. */
  async cancel(id: string): Promise<void> {
    await this.req("post", `/v1/tasks/${id}/cancel`);
  }

  /** List a task's artifacts (latest attempt) with metadata. */
  async artifacts(id: string): Promise<ArtifactMeta[]> {
    return this.req("get", `/v1/tasks/${id}/artifacts`);
  }

  /** Download a named artifact's raw content as a string. */
  async artifact(id: string, name: string): Promise<string> {
    const headers: Record<string, string> = {};
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;
    for (const base of this.bases.slice()) {
      let resp: Response;
      try {
        resp = await fetch(
          `${base}/v1/tasks/${id}/artifacts/${encodeURIComponent(name)}`,
          { headers },
        );
      } catch {
        continue;
      }
      if (resp.status === 503) {
        this.prefer(resp.headers.get("x-agentgrid-leader"));
        continue;
      }
      if (!resp.ok) throw new Error(`agentgrid artifact ${name} -> ${resp.status}`);
      return resp.text();
    }
    throw new Error(`agentgrid artifact ${name}: no reachable control plane`);
  }
}
