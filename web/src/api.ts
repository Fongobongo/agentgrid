// API client for the agentgrid control plane (Stage 2.5 cookie auth).
// The JWT travels in an HttpOnly + SameSite=Strict cookie set by /v1/auth/login
// (and /setup); all requests send `credentials: include` so the cookie rides
// along. No token is stored in localStorage (XSS-safe). An in-memory flag tracks
// whether the browser is authed so the UI can show the login screen.

let authed = false;
export function isAuthed(): boolean { return authed; }
export function markAuthed() { authed = true; }
export function markUnauthed() { authed = false; }

// Backwards-compatible names retained from the old localStorage API (callers use
// markAuthed/markUnauthed now); kept as no-ops so imports don't break.
export function getToken(): string | null { return null; }
export function setToken(_t: string) { markAuthed(); }
export function clearToken() { markUnauthed(); }

export interface TaskView {
  id: string;
  repository: string;
  prompt: string;
  adapter: string;
  status: string;
  created_at: string;
  finished_at: string | null;
  assigned_attempt_id: string | null;
  validation_command?: string | null;
}

export interface NodeView {
  id: string;
  name: string;
  status: string;
  adapters: string[];
  repositories: string[];
  max_concurrency: number;
  active_attempts: number;
  last_heartbeat_at: string;
  agent_version: string;
  load_avg: number;
  free_disk_mb: number;
}

export interface RepositoryView {
  id: string;
  name: string;
  git_url: string;
  default_branch: string;
  validation_command: string | null;
  created_at: string;
}

export interface ApprovalView {
  id: string;
  task_id: string;
  attempt_id: string;
  session_id?: string | null;
  permission: string;
  status: 'pending' | 'allowed' | 'denied' | 'expired' | 'cancelled';
  reason?: string | null;
  scope: string;
  created_at: string;
  expires_at: string;
  decided_at?: string | null;
}

export interface SkillTrustView {
  name: string;
  source: string;
  trusted: boolean;
  decided_by?: string | null;
  decided_at?: string | null;
}

export interface NodeEligibility {
  node_id: string;
  status: string;
  eligible: boolean;
  reasons: string[];
}

export interface TaskEligibility {
  task_id: string;
  no_eligible_nodes: string[];
  nodes: NodeEligibility[];
}

export interface TaskEvent {
  attempt_id: string;
  sequence: number;
  type: string;
  payload: any;
  created_at: string;
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

async function req(method: string, path: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const r = await fetch(path, {
    method,
    headers,
    credentials: 'include',
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (r.status === 401 && !path.startsWith('/v1/auth/')) {
    // Cookie expired/invalid: drop the in-memory auth flag and reload to login.
    markUnauthed();
    if (typeof window !== 'undefined') window.location.reload();
  }
  return r;
}

export async function getJson<T>(path: string): Promise<T> {
  const r = await req('GET', path);
  if (!r.ok) throw new ApiError(r.status, `GET ${path} -> ${r.status}`);
  return r.json();
}

export async function postJson<T>(path: string, body: unknown): Promise<T> {
  const r = await req('POST', path, body);
  if (!r.ok) throw new ApiError(r.status, `POST ${path} -> ${r.status}`);
  return r.json();
}

export function login(username: string, password: string) {
  return postJson<{ token: string }>('/v1/auth/login', { username, password }).then((r) => { markAuthed(); return r; });
}

export function setup(username: string, password: string) {
  return postJson<{ token: string }>('/v1/auth/setup', { username, password }).then((r) => { markAuthed(); return r; });
}

export function logout() {
  // Clear the HttpOnly cookie server-side; the browser cannot read/delete it directly.
  return fetch('/v1/auth/logout', { method: 'POST', credentials: 'include' }).finally(() => markUnauthed());
}

export function createTask(body: unknown) {
  return postJson<TaskView>('/v1/tasks', body);
}

export function getTask(id: string) {
  return getJson<TaskView>(`/v1/tasks/${id}`);
}

export function listTasks() {
  return getJson<TaskView[]>('/v1/tasks');
}

export function listNodes() {
  return getJson<NodeView[]>('/v1/nodes');
}

export function listRepos() {
  return getJson<RepositoryView[]>('/v1/repositories');
}

export function getEligibility(id: string) {
  return getJson<TaskEligibility>(`/v1/tasks/${id}/eligibility`);
}

export function getTaskEvents(taskId: string, after: number) {
  return getJson<TaskEvent[]>(`/v1/tasks/${taskId}/events?after_sequence=${after}`);
}

export function revokeNode(id: string) {
  return req('DELETE', `/v1/nodes/${id}`);
}

export interface WorkflowRun {
  id: string;
  template_id: string;
  status: string;
  created_at: string;
  finished_at: string | null;
  context?: string | null;
  repository?: string | null;
  base_commit?: string | null;
}

export interface StepProjection {
  step_id: string;
  role: string;
  status: string;
  depends_on: string[];
  requested_node_id?: string | null;
  task_id?: string | null;
  node_id?: string | null;
  attempts: number;
  verdict: string;
  error_code?: string | null;
  started_at?: string | null;
  finished_at?: string | null;
}

export interface BudgetUsage {
  messages: number;
  rounds: number;
  bytes: number;
  tokens: number;
  cost_cents: number;
  wall_seconds: number;
  repeated_handoffs: number;
}

export interface BudgetBreach {
  field: string;
  limit: number;
  observed: number;
}

export interface WorkflowBudget {
  max_messages?: number | null;
  max_rounds?: number | null;
  max_bytes?: number | null;
  max_tokens?: number | null;
  max_cost_cents?: number | null;
  max_wall_seconds?: number | null;
  max_repeated_handoffs?: number | null;
}

export interface BudgetSnapshot {
  limits: WorkflowBudget;
  usage: BudgetUsage;
  breach: BudgetBreach | null;
}

export interface WorkflowProjection {
  run: WorkflowRun;
  steps: StepProjection[];
  budget?: BudgetSnapshot | null;
}

export function listWorkflowRuns() {
  return getJson<WorkflowRun[]>('/v1/workflow-runs');
}

export function getWorkflowProjection(id: string) {
  return getJson<WorkflowProjection>(`/v1/workflow-runs/${id}/projection`);
}

export function cancelWorkflowRun(id: string) {
  return req('POST', `/v1/workflow-runs/${id}/cancel`, {});
}

export function approveWorkflowPlan(id: string) {
  return req('POST', `/v1/workflow-runs/${id}/approve-plan`, {});
}

export function cancelTask(id: string) {
  return req('POST', `/v1/tasks/${id}/cancel`, {});
}

export function retryTask(id: string) {
  return req('POST', `/v1/tasks/${id}/retry`, {});
}

export function listApprovals(status?: string) {
  return getJson<ApprovalView[]>(status ? `/v1/approvals?status=${encodeURIComponent(status)}` : '/v1/approvals');
}

export function answerApproval(id: string, decision: 'allow' | 'deny', reason?: string) {
  return req('POST', `/v1/approvals/${id}/${decision}`, reason ? { reason } : {});
}

export function listSkills(source?: string) {
  return getJson<SkillTrustView[]>(source ? `/v1/skills?source=${encodeURIComponent(source)}` : '/v1/skills');
}

export function setSkillTrust(name: string, source: string, trusted: boolean) {
  const dec = trusted ? 'trust' : 'untrust';
  return req('POST', `/v1/skills/${encodeURIComponent(name)}/${dec}?source=${encodeURIComponent(source)}`);
}

export async function getArtifact(taskId: string, name: string): Promise<string | null> {
  const r = await req('GET', `/v1/tasks/${taskId}/artifacts/${name}`);
  if (r.status === 404) return null;
  if (!r.ok) throw new ApiError(r.status, `GET artifact -> ${r.status}`);
  return r.text();
}

/// Stream a task's events over SSE with automatic reconnect + resume by
/// sequence, so a dropped connection never loses or duplicates events.
export interface StreamHandle {
  close: () => void;
}

export function streamTask(
  taskId: string,
  opts: {
    after?: number;
    onEvent: (e: TaskEvent) => void;
    onError?: (err: Error) => void;
  },
): StreamHandle {
  let lastSeq = opts.after ?? 0;
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let backoff = 500;

  const schedule = (fn: () => void) => {
    timer = setTimeout(fn, backoff);
    backoff = Math.min(backoff * 2, 5000);
  };

  const run = async () => {
    if (closed) return;
    try {
      const r = await fetch(
        `/v1/tasks/${taskId}/events/stream?after_sequence=${lastSeq}`,
        { credentials: 'include' },
      );
      if (!r.ok || !r.body) throw new ApiError(r.status, `stream -> ${r.status}`);
      backoff = 500;
      const reader = r.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';
      while (!closed) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx: number;
        while ((idx = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, idx).trim();
          buf = buf.slice(idx + 1);
          if (line.startsWith('data:')) {
            const data = line.slice(5).trim();
            if (!data) continue;
            try {
              const e = JSON.parse(data) as TaskEvent;
              if (e.sequence > lastSeq) lastSeq = e.sequence;
              opts.onEvent(e);
            } catch {
              /* ignore malformed */
            }
          }
        }
      }
    } catch (err) {
      if (closed) return;
      opts.onError?.(err as Error);
      if (!closed) schedule(run);
      return;
    }
    // Stream closed by server: resume from lastSeq to stay live.
    if (!closed) schedule(run);
  };

  run();
  return {
    close() {
      closed = true;
      if (timer) clearTimeout(timer);
    },
  };
}
