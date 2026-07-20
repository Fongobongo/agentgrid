// API client for the agentgrid control plane (Stage 4.3).

const TOKEN_KEY = 'agentgrid_token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}
export function setToken(t: string) {
  localStorage.setItem(TOKEN_KEY, t);
}
export function clearToken() {
  localStorage.removeItem(TOKEN_KEY);
}

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
  const t = getToken();
  if (t) headers['Authorization'] = `Bearer ${t}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  return fetch(path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
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
  return postJson<{ token: string }>('/v1/auth/login', { username, password });
}

export function setup(username: string, password: string) {
  return postJson<{ token: string }>('/v1/auth/setup', { username, password });
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
}

export interface WorkflowProjection {
  run: WorkflowRun;
  steps: StepProjection[];
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

export function cancelTask(id: string) {
  return req('POST', `/v1/tasks/${id}/cancel`, {});
}

export function retryTask(id: string) {
  return req('POST', `/v1/tasks/${id}/retry`, {});
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
    const t = getToken();
    try {
      const r = await fetch(
        `/v1/tasks/${taskId}/events/stream?after_sequence=${lastSeq}`,
        { headers: t ? { Authorization: `Bearer ${t}` } : {} },
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
