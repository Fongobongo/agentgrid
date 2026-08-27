// API client for the agentgrid control plane (Stage 2.5 cookie auth).
// The JWT travels in an HttpOnly + SameSite=Strict cookie set by /v1/auth/login
// (and /setup); all requests send `credentials: include` so the cookie rides
// along. No token is stored in localStorage (XSS-safe). An in-memory flag tracks
// whether the browser is authed so the UI can show the login screen.

let authed = false;
export function isAuthed(): boolean { return authed; }
export function markAuthed() { authed = true; }
export function markUnauthed() { authed = false; }

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
  // Hardening P2 item 36: security profile of the latest attempt.
  security_profile?: string | null;
  // Competitor-gap feature (GitHub write-back): informational echo.
  github_repo?: string | null;
  github_issue?: number | null;
  github_base_ref?: string | null;
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
  // Hardening P0 item 5: unsafe mode + permission interception.
  unsafe_active?: boolean;
  permission_interception?: string;
  // Hardening P2 item 35: local storage pressure.
  outbox_bytes?: number;
  artifact_spool_bytes?: number;
  // Hardening P2 item 37: maintenance drain — no NEW assignments.
  drained?: boolean;
  // Feature "opencode profiles": the opencode profile this node applies.
  opencode_profile_id?: string | null;
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
  // Hardening P0 item 9: global monotonic ingest cursor (0 on old servers).
  ingest_id: number;
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// Audit X-D5: exported so components that need raw status handling (e.g.
// 204 No Content deletes) still route through the central 401 handling
// instead of hand-rolled fetch() calls.
export async function req(method: string, path: string, body?: unknown): Promise<Response> {
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

/**
 * Await a mutation response and throw a typed ApiError on non-OK. Audit
 * follow-up: every view hand-rolled `if (!r.ok) setError(new Error(...))`
 * with drifted messages that lost the status code; callers now just
 * `await reqOk(...)` inside their existing try/catch.
 */
export async function reqOk(r: Response): Promise<Response> {
  if (!r.ok) throw new ApiError(r.status, `${r.url.replace(location.origin, '')} -> ${r.status}`);
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

export function setup(username: string, password: string, setupToken: string) {
  return postJson<{ token: string }>('/v1/auth/setup', { username, password, setup_token: setupToken }).then((r) => { markAuthed(); return r; });
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

interface ListResponse<T> { items: T[]; next_cursor?: string | null }

/**
 * GET a list endpoint, auto-paging through the keyset cursor until the
 * server reports no more pages. Audit follow-up: `next_cursor` was declared
 * but never read, so every list silently truncated at the server's default
 * page cap (~100 rows) with no indication. The cursor is the opaque
 * `"created_at,id"` pair the server emits; it round-trips as the
 * `after_created_at` / `after_id` query parts. A bare-array response (old
 * servers) stops paging after one request.
 */
async function listGet<T>(path: string, limit?: number): Promise<T[]> {
  const out: T[] = [];
  let cursor: string | null = null;
  let first = true;
  while (first || cursor) {
    const params = new URLSearchParams();
    const base = path.includes('?') ? path.slice(0, path.indexOf('?')) : path;
    for (const [k, v] of new URLSearchParams(path.includes('?') ? path.slice(path.indexOf('?') + 1) : '')) {
      params.set(k, v);
    }
    if (limit !== undefined && first) params.set('limit', String(limit));
    if (cursor) {
      const idx = cursor.indexOf(',');
      if (idx <= 0) break; // malformed cursor — stop rather than loop
      params.set('after_created_at', cursor.slice(0, idx));
      params.set('after_id', cursor.slice(idx + 1));
    }
    const raw = await getJson<unknown>(`${base}?${params.toString()}`);
    first = false;
    if (Array.isArray(raw)) {
      out.push(...(raw as T[]));
      break;
    }
    const resp = raw as ListResponse<T>;
    if (!resp || !Array.isArray(resp.items)) break;
    out.push(...resp.items);
    cursor = resp.next_cursor ?? null;
  }
  return out;
}

export function listTasks(limit?: number): Promise<TaskView[]> {
  return listGet<TaskView>('/v1/tasks', limit);
}

/** Plan 1.3: FTS5 full-text search over tasks. */
export function searchTasks(q: string): Promise<TaskView[]> {
  return listGet<TaskView>(`/v1/search?q=${encodeURIComponent(q)}`);
}

export interface EventSearchHit {
  task_id: string;
  attempt_id: string;
  sequence: number;
  event_type: string;
  payload: string;
}

export function searchEvents(q: string): Promise<EventSearchHit[]> {
  return listGet<EventSearchHit>(`/v1/search/events?q=${encodeURIComponent(q)}`);
}

export function listNodes(): Promise<NodeView[]> {
  return listGet<NodeView>('/v1/nodes');
}

export function listRepos(): Promise<RepositoryView[]> {
  return listGet<RepositoryView>('/v1/repositories');
}

export function getEligibility(id: string) {
  return getJson<TaskEligibility>(`/v1/tasks/${id}/eligibility`);
}

export function getTaskEvents(taskId: string, after?: number) {
  // Hardening P0 item 9: resume on the global ingest cursor (0 = from start).
  const q = after && after > 0 ? `?after_ingest=${after}` : '';
  return getJson<TaskEvent[]>(`/v1/tasks/${taskId}/events${q}`);
}

export function revokeNode(id: string) {
  return req('DELETE', `/v1/nodes/${id}`);
}

// Hardening P2 item 37: drain (stop NEW assignments) / undrain a node.
export function drainNode(id: string, drain: boolean) {
  return req('POST', `/v1/nodes/${id}/drain?drain=${drain}`);
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
  // Plan 3.3: step outcome surfaced without the CLI.
  prompt?: string | null;
  commit_sha?: string | null;
  result?: string | null;
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
  pending_plan?: string | null;
}

export function listWorkflowRuns(): Promise<WorkflowRun[]> {
  return listGet<WorkflowRun>('/v1/workflow-runs');
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

// Competitor plan 1.1: pending patch-review approval for a task, or null.
export function getTaskReviewApproval(taskId: string) {
  return getJson<ApprovalView | null>(`/v1/tasks/${taskId}/review-approval`);
}

// Competitor-gap feature (diff review): inline annotations on an attempt's
// diff/plan, plus "send for rework" — a fresh task with the annotations
// folded into the prompt.
export interface PatchAnnotation {
  id: string;
  attempt_id: string;
  file: string;
  line_start: number | null;
  line_end: number | null;
  comment: string;
  created_at: string;
}

export interface CreateAnnotationRequest {
  file: string;
  line_start?: number | null;
  line_end?: number | null;
  comment: string;
}

export interface ReworkResponse {
  task_id: string;
}

export function listAnnotations(attemptId: string): Promise<PatchAnnotation[]> {
  return getJson<PatchAnnotation[]>(`/v1/attempts/${attemptId}/annotations`);
}

export function addAnnotation(attemptId: string, body: CreateAnnotationRequest): Promise<PatchAnnotation> {
  return postJson<PatchAnnotation>(`/v1/attempts/${attemptId}/annotations`, body);
}

export function reworkAttempt(attemptId: string): Promise<ReworkResponse> {
  return postJson<ReworkResponse>(`/v1/attempts/${attemptId}/rework`, {});
}

export function listApprovals(status?: string): Promise<ApprovalView[]> {
  return listGet<ApprovalView>(status ? `/v1/approvals?status=${encodeURIComponent(status)}` : '/v1/approvals');
}

export function answerApproval(id: string, decision: 'allow' | 'deny', reason?: string) {
  return req('POST', `/v1/approvals/${id}/${decision}`, reason ? { reason } : {});
}

export function listSkills(source?: string): Promise<SkillTrustView[]> {
  return listGet<SkillTrustView>(source ? `/v1/skills?source=${encodeURIComponent(source)}` : '/v1/skills');
}

export function setSkillTrust(name: string, source: string, trusted: boolean) {
  const dec = trusted ? 'trust' : 'untrust';
  return req('POST', `/v1/skills/${encodeURIComponent(name)}/${dec}?source=${encodeURIComponent(source)}`);
}

// Hardening P2 item 36: artifact download returns { text, sha256 } — the
// server-computed integrity hash is exposed for display.
export interface ArtifactDownload {
  text: string;
  sha256: string | null;
}

export async function getArtifact(taskId: string, name: string): Promise<ArtifactDownload | null> {
  const r = await req('GET', `/v1/tasks/${taskId}/artifacts/${name}`);
  if (r.status === 404) return null;
  if (!r.ok) throw new ApiError(r.status, `GET artifact -> ${r.status}`);
  const text = await r.text();
  return { text, sha256: r.headers.get('x-artifact-sha256') };
}

/// Stream a task's events over SSE with automatic reconnect + resume by
/// sequence, so a dropped connection never loses or duplicates events.
export interface StreamHandle {
  close: () => void;
}

// Audit X-D5b: shared SSE reconnect loop. Both streams used to carry
// near-identical copies of the backoff schedule, the 401→login handling and
// the line-buffered reader pump; they drifted (only one surfaced errors).
// `path` may be a function so task streams can rebuild their resume cursor
// after every reconnect.
function sseConnect(
  path: string | (() => string),
  onLine: (line: string) => void,
  onError?: (err: Error) => void,
): StreamHandle {
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let backoff = 500;
  let activeReader: { cancel: () => Promise<unknown> } | null = null;

  const schedule = (fn: () => void) => {
    timer = setTimeout(fn, backoff);
    backoff = Math.min(backoff * 2, 5000);
  };

  const run = async () => {
    if (closed) return;
    try {
      const p = typeof path === 'function' ? path() : path;
      const r = await fetch(p, { credentials: 'include' });
      if (r.status === 401) {
        // Cookie expired: same handling as req() — back to login, no retry loop.
        markUnauthed();
        if (typeof window !== 'undefined') window.location.reload();
        return;
      }
      if (!r.ok || !r.body) throw new ApiError(r.status, `stream -> ${r.status}`);
      backoff = 500;
      const reader = r.body.getReader();
      activeReader = reader;
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
          onLine(line);
        }
      }
    } catch (err) {
      if (closed) return;
      onError?.(err as Error);
      if (!closed) schedule(run);
      return;
    }
    // Stream closed by server: resume from the caller's cursor to stay live.
    if (!closed) schedule(run);
  };

  run();
  return {
    close() {
      closed = true;
      if (timer) clearTimeout(timer);
      // Cancel the in-flight read so the pending reader.read() resolves
      // immediately — otherwise the socket stayed open until the next
      // server keep-alive (up to ~15 s) after route switches.
      activeReader?.cancel().catch(() => {});
    },
  };
}

export function streamTask(
  taskId: string,
  opts: {
    after?: number;
    onEvent: (e: TaskEvent) => void;
    onError?: (err: Error) => void;
  },
): StreamHandle {
  let lastIngest = opts.after ?? 0;
  // Hardening P0 item 9: resume on the global ingest cursor so a retry never
  // reorders or re-delivers events across attempts.
  return sseConnect(
    () => `/v1/tasks/${taskId}/events/stream?after_ingest=${lastIngest}`,
    (line) => {
      if (!line.startsWith('data:')) return;
      const data = line.slice(5).trim();
      if (!data) return;
      try {
        const e = JSON.parse(data) as TaskEvent;
        if (e.ingest_id > lastIngest) lastIngest = e.ingest_id;
        opts.onEvent(e);
      } catch {
        /* ignore malformed */
      }
    },
    opts.onError,
  );
}

// Plan 3.4: audit trail (who decided what) with an action filter.
export interface AuditEvent {
  id: string;
  actor_type: string;
  actor_id: string | null;
  action: string;
  subject: string | null;
  payload: string | null;
  created_at: string;
}

export function listAudit(action?: string, limit = 100): Promise<AuditEvent[]> {
  const q = new URLSearchParams();
  if (action) q.set('action', action);
  q.set('limit', String(limit));
  return listGet<AuditEvent>(`/v1/audit?${q.toString()}`);
}

// Plan 3.2: change-notification stream. The server emits `hello` on connect
// and `change` whenever the task/node/workflow-run status fingerprint moves;
// lists refetch only on those events, so an idle UI makes zero requests.
export function streamChanges(onChange: () => void): StreamHandle {
  return sseConnect('/v1/stream', (line) => {
    // hello/change frames both mean "lists may have moved".
    if (line.startsWith('event:') && /hello|change/.test(line)) onChange();
  });
}


// Feature "opencode profiles": control-plane-managed opencode config.
export interface OpencodeProfile {
  id: string;
  name: string;
  hash: string;
  config: Record<string, unknown>;
  /** Previous revision (profile revisions feature). */
  prev?: { hash: string; config: Record<string, unknown> } | null;
  expires_at?: string | null;
  apply_count?: number | null;
  pinned_skills?: string[] | null;
  created_at: string;
  updated_at: string;
}

export interface OpencodeAuditEntry {
  id: string;
  node_id: string;
  profile_id: string | null;
  hash: string;
  trigger: string;
  at: string;
}

export function listOpencodeProfiles(): Promise<OpencodeProfile[]> {
  return listGet<OpencodeProfile>('/v1/opencode-profiles');
}

export function getOpencodeAudit(nodeId: string): Promise<OpencodeAuditEntry[]> {
  return listGet<OpencodeAuditEntry>(`/v1/nodes/${nodeId}/opencode-audit`);
}
