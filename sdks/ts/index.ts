// Agentgrid TypeScript SDK (plan 1.11 / roadmap #8).
//
// Thin client over the /v1 HTTP API — no dependencies, Node >= 18 (fetch).
// Auth: pass a JWT token (from `ag login` or `POST /v1/auth/login`) or set
// AGENTGRID_TOKEN. Usage:
//
//   import { Agentgrid } from "@agentgrid/sdk";
//   const ag = new Agentgrid("http://127.0.0.1:7800", process.env.AGENTGRID_TOKEN!);
//   const task = await ag.run("fix the flaky test", "my-org/my-repo");
//   await ag.wait(task.id);                 // poll until terminal
//   const arts = await ag.artifacts(task.id);
//   console.log(await ag.artifact(task.id, arts[0].name));
//
// Minimal surface: run | wait | cancel | artifacts | artifact | status.

export interface Task {
  id: string;
  repository: string;
  prompt: string;
  adapter: string;
  status: string;
  created_at: string;
  finished_at: string | null;
  assigned_attempt_id: string | null;
}

export interface ArtifactMeta {
  name: string;
  size_bytes: number;
  media_type: string | null;
  sha256: string | null;
}

const TERMINAL = new Set(["succeeded", "failed", "cancelled", "blocked"]);

export class Agentgrid {
  private token: string;
  private bases: string[];
  constructor(
    base: string | string[],
    token?: string,
  ) {
    this.bases = (Array.isArray(base) ? base : [base]).map((b) =>
      b.replace(/\/+$/, ""),
    );
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
      return resp.json();
    }
    throw lastError;
  }

  /** Login and return the JWT (same as `ag login`). */
  async login(username: string, password: string): Promise<string> {
    const r = await this.req("post", "/v1/auth/login", { username, password });
    this.token = r.token;
    return r.token;
  }

  /** Run a task: `run(prompt, repository, opts?) -> Task`. */
  async run(
    prompt: string,
    repository: string,
    opts: {
      adapter?: string;
      requested_node_id?: string;
      timeout_secs?: number;
      validation_command?: string;
      base_commit?: string;
    } = {},
  ): Promise<Task> {
    return this.req("post", "/v1/tasks", {
      prompt,
      repository,
      adapter: opts.adapter ?? "mock",
      requested_node_id: opts.requested_node_id ?? null,
      timeout_secs: opts.timeout_secs ?? null,
      validation_command: opts.validation_command ?? null,
      base_commit: opts.base_commit ?? null,
    });
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
