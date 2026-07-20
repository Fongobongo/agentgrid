//! Stage 6 northbound gateway: `GatewayAgent` speaks the ACP *agent* role and
//! maps each ACP session to an Agentgrid task. `session/new` mints a session
//! id; `session/prompt` creates the task (prompt known only here), then polls
//! the task's events and streams them back as `session/update` until the task
//! reaches a terminal state; `session/cancel` cancels the underlying task.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use agentgrid_common::{ApprovalView, CreateTaskRequest, TaskEvent, TaskStatus, TaskView};
use serde_json::Value;

use crate::codec::RpcError;
use crate::methods::{
    InitializeParams, SessionCancelParams, SessionNewParams, SessionPromptParams,
    METHOD_SESSION_REQUEST_PERMISSION,
};
use crate::server::{notify_update, AcpAgent, AcpCtx};

#[derive(Clone)]
struct SessionMeta {
    agent: String,
    #[allow(dead_code)]
    model: Option<String>,
    #[allow(dead_code)]
    cwd: String,
    task_id: Option<String>,
}

/// ACP agent that bridges external ACP clients to the Agentgrid control plane.
pub struct GatewayAgent {
    client: reqwest::Client,
    server: String,
    token: Option<String>,
    sessions: Mutex<HashMap<String, SessionMeta>>,
    /// Approval ids already surfaced to the ACP client (each asked exactly once).
    asked: Mutex<HashSet<String>>,
}

impl GatewayAgent {
    pub fn new(server: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            server,
            token,
            sessions: Mutex::new(HashMap::new()),
            asked: Mutex::new(HashSet::new()),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.header("Authorization", format!("Bearer {t}")),
            None => req,
        }
    }

    async fn create_task(&self, req: &CreateTaskRequest) -> Result<String, RpcError> {
        let resp = self
            .auth(self.client.post(format!("{}/v1/tasks", self.server)))
            .json(req)
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!("create_task {}", resp.status()),
                data: None,
            });
        }
        let view: TaskView = resp.json().await.map_err(http_err)?;
        Ok(view.id)
    }

    async fn get_task(&self, task_id: &str) -> Result<TaskView, RpcError> {
        let resp = self
            .auth(
                self.client
                    .get(format!("{}/v1/tasks/{}", self.server, task_id)),
            )
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!("get_task {}", resp.status()),
                data: None,
            });
        }
        resp.json().await.map_err(http_err)
    }

    async fn get_events(&self, task_id: &str, after: u64) -> Result<Vec<TaskEvent>, RpcError> {
        let resp = self
            .auth(
                self.client
                    .get(format!("{}/v1/tasks/{}/events", self.server, task_id)),
            )
            .query(&[("after_sequence", after)])
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!("get_events {}", resp.status()),
                data: None,
            });
        }
        resp.json().await.map_err(http_err)
    }

    async fn get_pending_approvals(&self) -> Result<Vec<ApprovalView>, RpcError> {
        let resp = self
            .auth(self.client.get(format!("{}/v1/approvals", self.server)))
            .query(&[("status", "pending")])
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!("get_approvals {status}", status = resp.status()),
                data: None,
            });
        }
        resp.json().await.map_err(http_err)
    }

    async fn decide_approval(&self, id: &str, allow: bool) -> Result<(), RpcError> {
        let verb = if allow { "allow" } else { "deny" };
        let resp = self
            .auth(
                self.client
                    .post(format!("{}/v1/approvals/{id}/{verb}", self.server)),
            )
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!(
                    "decide_approval {verb} {id}: {status}",
                    status = resp.status()
                ),
                data: None,
            });
        }
        Ok(())
    }

    async fn cancel_task(&self, task_id: &str) -> Result<(), RpcError> {
        let resp = self
            .auth(
                self.client
                    .post(format!("{}/v1/tasks/{}/cancel", self.server, task_id)),
            )
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(RpcError {
                code: -32000,
                message: format!("cancel_task {}", resp.status()),
                data: None,
            });
        }
        Ok(())
    }
}

fn http_err(e: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: -32000,
        message: format!("transport: {e}"),
        data: None,
    }
}

/// GET a control-plane path and return the JSON body verbatim (used by the
/// `_agentgrid/*` extension methods).
async fn get_json(agent: &GatewayAgent, path: &str) -> Result<Value, RpcError> {
    let resp = agent
        .auth(agent.client.get(format!("{}{}", agent.server, path)))
        .send()
        .await
        .map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(RpcError {
            code: -32000,
            message: format!("get_json {path}: {status}", status = resp.status()),
            data: None,
        });
    }
    resp.json().await.map_err(http_err)
}

fn is_terminal(s: &TaskStatus) -> bool {
    matches!(
        s,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

impl AcpAgent for GatewayAgent {
    async fn initialize(&self, _p: InitializeParams) -> Result<Value, RpcError> {
        Ok(serde_json::json!({
            "protocol_version": "0.1",
            "capabilities": {},
            "client": {},
        }))
    }

    async fn session_new(&self, p: SessionNewParams) -> Result<Value, RpcError> {
        let sid = format!("ag-{}", uuid::Uuid::new_v4());
        self.sessions.lock().unwrap().insert(
            sid.clone(),
            SessionMeta {
                agent: p.agent,
                model: p.model,
                cwd: p.cwd,
                task_id: None,
            },
        );
        Ok(serde_json::json!({ "session_id": sid }))
    }

    async fn session_prompt(&self, ctx: AcpCtx, p: SessionPromptParams) -> Result<Value, RpcError> {
        let meta = self.sessions.lock().unwrap().get(&p.session_id).cloned();
        let meta = match meta {
            Some(m) => m,
            None => {
                return Err(RpcError {
                    code: -32000,
                    message: "unknown session".into(),
                    data: None,
                })
            }
        };
        let req = CreateTaskRequest {
            prompt: p.prompt,
            repository: "*".into(),
            adapter: meta.agent,
            requested_node_id: None,
            timeout_secs: Some(3600),
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        };
        let task_id = self.create_task(&req).await?;
        if let Some(m) = self.sessions.lock().unwrap().get_mut(&p.session_id) {
            m.task_id = Some(task_id.clone());
        }
        let mut after: u64 = 0;
        loop {
            let events = self.get_events(&task_id, after).await?;
            for e in &events {
                notify_update(&ctx.sender, &p.session_id, e.payload.clone());
                after = after.max(e.sequence);
            }

            // Surface pending permission requests to the ACP client and relay
            // its decision back to the control plane (node → CP → ACP client).
            if let Ok(approvals) = self.get_pending_approvals().await {
                for a in approvals {
                    if a.task_id != task_id {
                        continue;
                    }
                    if !self.asked.lock().unwrap().insert(a.id.clone()) {
                        continue;
                    }
                    let permission: Value = serde_json::from_str(&a.permission)
                        .unwrap_or(Value::String(a.permission.clone()));
                    let decision = ctx
                        .request(
                            METHOD_SESSION_REQUEST_PERMISSION,
                            serde_json::json!({
                                "session_id": p.session_id,
                                "permission": permission,
                            }),
                        )
                        .await
                        .ok()
                        .and_then(|v| v.get("allowed").and_then(|b| b.as_bool()))
                        .unwrap_or(false);
                    let _ = self.decide_approval(&a.id, decision).await;
                }
            }

            let view = self.get_task(&task_id).await?;
            if is_terminal(&view.status) {
                return Ok(serde_json::json!({
                    "status": serde_json::to_value(view.status).unwrap_or(Value::Null),
                    "error_code": view.error_code,
                }));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn session_cancel(&self, p: SessionCancelParams) -> Result<Value, RpcError> {
        let task_id = self
            .sessions
            .lock()
            .unwrap()
            .get(&p.session_id)
            .and_then(|m| m.task_id.clone());
        match task_id {
            Some(t) => {
                self.cancel_task(&t).await?;
                Ok(serde_json::json!({}))
            }
            // Nothing created yet; nothing to cancel.
            None => Ok(serde_json::json!({})),
        }
    }

    async fn handle_extension(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "_agentgrid/nodes" => get_json(self, "/v1/nodes").await,
            "_agentgrid/task_eligibility" => {
                let task_id = params
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError {
                        code: -32602,
                        message: "task_eligibility requires task_id".into(),
                        data: None,
                    })?;
                get_json(self, &format!("/v1/tasks/{task_id}/eligibility")).await
            }
            "_agentgrid/workflow/projection" => {
                let run_id = params
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError {
                        code: -32602,
                        message: "workflow/projection requires run_id".into(),
                        data: None,
                    })?;
                get_json(self, &format!("/v1/workflow-runs/{run_id}/projection")).await
            }
            other => Err(RpcError {
                code: -32601,
                message: format!("unknown extension method: {other}"),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_new_mints_id_and_stores_meta() {
        let agent = GatewayAgent::new("http://127.0.0.1:1".into(), None);
        let res = agent
            .session_new(SessionNewParams {
                agent: "claude".into(),
                model: Some("opus".into()),
                cwd: "/tmp".into(),
                prompt: None,
                mcp: Value::Null,
                parent_session_id: None,
            })
            .await
            .unwrap();
        let sid = res.get("session_id").and_then(|s| s.as_str()).unwrap();
        assert!(sid.starts_with("ag-"));
        assert!(agent.sessions.lock().unwrap().contains_key(sid));
        // Unknown session -> error (no network touched).
        let err = agent
            .session_prompt(
                AcpCtx::new(tokio::sync::mpsc::unbounded_channel().0),
                SessionPromptParams {
                    session_id: "nope".into(),
                    prompt: "x".into(),
                },
            )
            .await;
        assert!(err.is_err());
    }
}

/// Full northbound path over real pipes + a tiny in-process fake control
/// plane: an ACP client drives the gateway, the gateway creates a task, the
/// fake CP streams two `session/update` events (plan + result), raises a
/// pending approval, and the gateway forwards it to the client and relays the
/// client's `allow` decision back to the CP before the task terminates.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use agentgrid_common::EventKind;
    use axum::{
        extract::{Path, Query, State},
        http::StatusCode,
        Json, Router,
    };
    use serde_json::{json, Value};
    use tokio::io::duplex;
    use tokio::net::TcpListener;
    use tokio::time::{timeout, Instant};

    use crate::client::new as acp_client_new;
    use crate::methods::{
        map_session_update, InitializeParams, SessionNewParams, SessionPromptParams,
    };
    use crate::server::AcpServer;

    struct FakeCp {
        task_id: String,
        delivered: AtomicBool,
        decided: Mutex<Vec<String>>,
    }

    async fn cp_create_task(State(s): State<Arc<FakeCp>>, Json(req): Json<Value>) -> Json<Value> {
        Json(json!({
            "id": s.task_id,
            "repository": req.get("repository").cloned().unwrap_or(Value::Null),
            "prompt": req.get("prompt").cloned().unwrap_or(Value::Null),
            "adapter": req.get("adapter").cloned().unwrap_or(Value::Null),
            "status": "running",
            "created_at": "t",
            "finished_at": null,
            "assigned_attempt_id": null,
            "validation_command": null,
            "error_code": null,
        }))
    }

    async fn cp_get_task(State(s): State<Arc<FakeCp>>, Path(id): Path<String>) -> Json<Value> {
        let delivered = s.delivered.load(Ordering::SeqCst);
        Json(json!({
            "id": id,
            "repository": "r",
            "prompt": "p",
            "adapter": "a",
            "status": if delivered { "succeeded" } else { "running" },
            "created_at": "t",
            "finished_at": null,
            "assigned_attempt_id": null,
            "validation_command": null,
            "error_code": null,
        }))
    }

    async fn cp_get_events(
        State(s): State<Arc<FakeCp>>,
        Path(_id): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Vec<Value>> {
        let after: u64 = q
            .get("after_sequence")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if after == 0 {
            s.delivered.store(true, Ordering::SeqCst);
            Json(vec![
                json!({"attempt_id":"att-1","sequence":1,"type":"status","payload":{"type":"plan","text":"planning"},"created_at":"t"}),
                json!({"attempt_id":"att-1","sequence":2,"type":"status","payload":{"type":"result","text":"done"},"created_at":"t"}),
            ])
        } else {
            Json(vec![])
        }
    }

    async fn cp_list_approvals(
        State(s): State<Arc<FakeCp>>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Vec<Value>> {
        if q.get("status").map(|x| x.as_str()) == Some("pending") {
            Json(vec![json!({
                "id": "apr-1",
                "task_id": s.task_id,
                "attempt_id": "att-1",
                "session_id": null,
                "permission": serde_json::to_string(&json!({"tool":"bash"})).unwrap(),
                "status": "pending",
                "reason": null,
                "created_at": "t",
                "expires_at": "t",
                "decided_at": null,
            })])
        } else {
            Json(vec![])
        }
    }

    async fn cp_allow(State(s): State<Arc<FakeCp>>, Path(id): Path<String>) -> StatusCode {
        s.decided.lock().unwrap().push(format!("allow:{id}"));
        StatusCode::OK
    }

    async fn cp_deny(State(s): State<Arc<FakeCp>>, Path(id): Path<String>) -> StatusCode {
        s.decided.lock().unwrap().push(format!("deny:{id}"));
        StatusCode::OK
    }

    async fn cp_list_nodes(State(_s): State<Arc<FakeCp>>) -> Json<Vec<Value>> {
        Json(vec![json!({
            "id": "node-1",
            "name": "n1",
            "status": "online",
            "adapters": ["claude"],
            "repositories": ["*"],
            "max_concurrency": 2,
            "active_attempts": 0,
            "last_heartbeat_at": "t",
            "agent_version": "0.1",
            "load_avg": 0.0,
            "free_disk_mb": 100000,
        })])
    }

    async fn cp_task_eligibility(
        State(_s): State<Arc<FakeCp>>,
        Path(id): Path<String>,
    ) -> Json<Value> {
        Json(json!({
            "task_id": id,
            "no_eligible_nodes": [],
            "nodes": [],
        }))
    }

    #[tokio::test]
    async fn gateway_streams_events_and_relays_approval() {
        let cp = Arc::new(FakeCp {
            task_id: "task-1".into(),
            delivered: AtomicBool::new(false),
            decided: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/v1/tasks", axum::routing::post(cp_create_task))
            .route("/v1/tasks/{id}", axum::routing::get(cp_get_task))
            .route("/v1/tasks/{id}/events", axum::routing::get(cp_get_events))
            .route("/v1/approvals", axum::routing::get(cp_list_approvals))
            .route("/v1/approvals/{id}/allow", axum::routing::post(cp_allow))
            .route("/v1/approvals/{id}/deny", axum::routing::post(cp_deny))
            .with_state(cp.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (c2s, s_read) = duplex(8192);
        let (s_write, c_read) = duplex(8192);
        let agent = GatewayAgent::new(format!("http://{addr}"), None);
        tokio::spawn(AcpServer::new(s_read, s_write, agent).run());

        let (client, mut notif) = acp_client_new(c_read, c2s);
        let client = Arc::new(client);
        client
            .initialize(InitializeParams {
                protocol_version: "0.1".into(),
                agent: "claude".into(),
                model: "v1".into(),
                session_id: None,
                cwd: "/tmp".into(),
                capabilities: json!({}),
                client: json!({}),
            })
            .await
            .unwrap();
        let new_res = client
            .session_new(SessionNewParams {
                agent: "claude".into(),
                model: None,
                cwd: "/tmp".into(),
                prompt: None,
                mcp: Value::Null,
                parent_session_id: None,
            })
            .await
            .unwrap();

        let sid = new_res.session_id.clone();
        let prompt = tokio::spawn({
            let c = client.clone();
            async move {
                c.session_prompt(SessionPromptParams {
                    session_id: sid,
                    prompt: "do it".into(),
                })
                .await
            }
        });

        let mut kinds = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(200), notif.recv()).await {
                match msg {
                    crate::codec::Message::Notification { params, .. } => {
                        if let (Some(s), Some(u)) = (
                            params.get("session_id").and_then(|x| x.as_str()),
                            params.get("update"),
                        ) {
                            kinds.push(map_session_update(s, u).kind);
                        }
                    }
                    crate::codec::Message::Request { id, .. } => {
                        // ACP client answers the permission prompt.
                        let _ = client.respond(id, true).await;
                    }
                    _ => {}
                }
            }
            if prompt.is_finished() || Instant::now() > deadline {
                break;
            }
        }

        let res = timeout(Duration::from_secs(10), prompt)
            .await
            .expect("prompt timed out")
            .unwrap()
            .unwrap();
        assert_eq!(res.get("status").unwrap(), &json!("succeeded"));
        assert!(kinds.contains(&EventKind::Plan));
        assert!(kinds.contains(&EventKind::Result));
        assert_eq!(*cp.decided.lock().unwrap(), vec!["allow:apr-1".to_string()]);
    }

    #[tokio::test]
    async fn gateway_exposes_extension_methods() {
        let cp = Arc::new(FakeCp {
            task_id: "task-x".into(),
            delivered: AtomicBool::new(false),
            decided: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/v1/nodes", axum::routing::get(cp_list_nodes))
            .route(
                "/v1/tasks/{id}/eligibility",
                axum::routing::get(cp_task_eligibility),
            )
            .with_state(cp.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (c2s, s_read) = duplex(8192);
        let (s_write, c_read) = duplex(8192);
        tokio::spawn(
            AcpServer::new(
                s_read,
                s_write,
                GatewayAgent::new(format!("http://{addr}"), None),
            )
            .run(),
        );

        let (client, _notif) = acp_client_new(c_read, c2s);
        let client = Arc::new(client);
        client
            .initialize(InitializeParams {
                protocol_version: "0.1".into(),
                agent: "claude".into(),
                model: "v1".into(),
                session_id: None,
                cwd: "/tmp".into(),
                capabilities: json!({}),
                client: json!({}),
            })
            .await
            .unwrap();

        let nodes = client
            .request("_agentgrid/nodes", json!({}))
            .await
            .expect("nodes extension");
        assert_eq!(
            nodes.as_array().unwrap()[0].get("id").unwrap(),
            &json!("node-1")
        );

        let elig = client
            .request(
                "_agentgrid/task_eligibility",
                json!({ "task_id": "task-x" }),
            )
            .await
            .expect("eligibility extension");
        assert_eq!(elig.get("task_id").unwrap(), &json!("task-x"));
        assert!(elig
            .get("no_eligible_nodes")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());

        // Unknown extension method is a clean RPC error, not a hang.
        assert!(client.request("_agentgrid/bogus", json!({})).await.is_err());
    }

    async fn cp_workflow_projection(Path(_id): Path<String>) -> Json<Value> {
        Json(json!({
            "run": {"id": "run-x", "status": "blocked"},
            "steps": [{"step_id": "a", "role": "integrator", "verdict": "failed"}]
        }))
    }

    #[tokio::test]
    async fn gateway_exposes_workflow_projection() {
        let cp = Arc::new(FakeCp {
            task_id: "task-x".into(),
            delivered: AtomicBool::new(false),
            decided: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route(
                "/v1/workflow-runs/{id}/projection",
                axum::routing::get(cp_workflow_projection),
            )
            .with_state(cp.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (c2s, s_read) = duplex(8192);
        let (s_write, c_read) = duplex(8192);
        tokio::spawn(
            AcpServer::new(
                s_read,
                s_write,
                GatewayAgent::new(format!("http://{addr}"), None),
            )
            .run(),
        );
        let (client, _notif) = acp_client_new(c_read, c2s);
        let client = Arc::new(client);
        client
            .initialize(InitializeParams {
                protocol_version: "0.1".into(),
                agent: "claude".into(),
                model: "v1".into(),
                session_id: None,
                cwd: "/tmp".into(),
                capabilities: json!({}),
                client: json!({}),
            })
            .await
            .unwrap();

        let proj = client
            .request(
                "_agentgrid/workflow/projection",
                json!({ "run_id": "run-x" }),
            )
            .await
            .expect("projection extension");
        assert_eq!(
            proj.get("run").unwrap().get("status").unwrap(),
            &json!("blocked")
        );
        assert_eq!(
            proj.get("steps").unwrap().as_array().unwrap()[0]
                .get("role")
                .unwrap(),
            &json!("integrator")
        );
    }
}
