//! ACP northbound server: lets Agentgrid (or any Rust process) speak the
//! *agent* side of ACP over any byte transport. An `AcpAgent` implements the
//! session methods; `AcpServer` decodes inbound JSON-RPC requests, dispatches
//! them, and streams `session/update` notifications back to the client.
//!
//! This is the mirror of `client::AcpClient` and the foundation for the
//! `agentgrid acp-agent` subcommand (Stage 6). Generic over the agent so no
//! `dyn`/`async_trait` dependency is needed.
#![allow(clippy::manual_async_fn)]

use crate::codec::{decode_line, encode_line, Id, Message, RpcError};
use crate::methods::{
    InitializeParams, SessionCancelParams, SessionNewParams, SessionPromptParams,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Handle the agent uses to stream `session/update` notifications to the
/// connected ACP client during a prompt turn, and to send agent→client
/// requests (e.g. `session/request_permission`) and await the response.
pub struct AcpCtx {
    pub sender: mpsc::UnboundedSender<Message>,
    pub(crate) pending: Arc<Mutex<HashMap<Id, oneshot::Sender<Message>>>>,
    pub(crate) idgen: Arc<AtomicU64>,
}

impl AcpCtx {
    /// Build a context with a fresh pending/request id space (used by tests
    /// that don't run through the server's shared response router).
    pub fn new(sender: mpsc::UnboundedSender<Message>) -> Self {
        Self {
            sender,
            pending: Arc::new(Mutex::new(HashMap::new())),
            idgen: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Send an agent→client request and await its response. Used to surface
    /// `session/request_permission` to the ACP client and read the decision.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = Id::Num(self.idgen.fetch_add(1, Ordering::SeqCst) as i64);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        self.sender
            .send(Message::Request {
                id: id.clone(),
                method: method.to_string(),
                params,
            })
            .map_err(|_| RpcError {
                code: -32000,
                message: "transport closed".into(),
                data: None,
            })?;
        let msg = rx.await.map_err(|_| RpcError {
            code: -32000,
            message: "transport closed".into(),
            data: None,
        })?;
        match msg {
            Message::Response { result, .. } => result,
            _ => Err(RpcError {
                code: -32603,
                message: "unexpected response shape".into(),
                data: None,
            }),
        }
    }
}

/// The agent side of ACP. Implemented by the northbound gateway (and by tests).
pub trait AcpAgent: Send + Sync {
    fn initialize(
        &self,
        params: InitializeParams,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send;
    fn session_new(
        &self,
        params: SessionNewParams,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send;
    fn session_prompt(
        &self,
        ctx: AcpCtx,
        params: SessionPromptParams,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send;
    fn session_cancel(
        &self,
        params: SessionCancelParams,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send;
    /// Custom `_`-prefixed extension methods (e.g. `_agentgrid/nodes`).
    /// Default: method not found, so non-gateway agents ignore them.
    fn handle_extension(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send {
        let _ = (method, params);
        async {
            Err(RpcError {
                code: -32601,
                message: "method not found".into(),
                data: None,
            })
        }
    }
    /// Inbound notifications (client→agent). Default: ignore.
    fn handle_notification(&self, _params: Value) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }
}

/// Push a `session/update` to the client. Safe to call from within
/// `session_prompt` via the `AcpCtx` sender.
pub fn notify_update(sender: &mpsc::UnboundedSender<Message>, session_id: &str, update: Value) {
    let _ = sender.send(Message::Notification {
        method: "session/update".into(),
        params: serde_json::json!({ "session_id": session_id, "update": update }),
    });
}

/// Server that reads requests from `reader`, writes responses/notifications to
/// `writer`, and dispatches the session lifecycle to `agent`.
pub struct AcpServer<R, W, A>
where
    A: AcpAgent + Send + Sync + 'static,
{
    reader: R,
    writer: W,
    agent: Arc<A>,
}

impl<R, W, A> AcpServer<R, W, A>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    A: AcpAgent + Send + Sync + 'static,
{
    pub fn new(reader: R, writer: W, agent: A) -> Self {
        Self {
            reader,
            writer,
            agent: Arc::new(agent),
        }
    }

    /// Run the server until the client closes the transport. Returns the
    /// reader-task handle (the writer task ends when the write channel closes).
    pub async fn run(self) {
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Message>();
        let mut writer = self.writer;
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if writer
                    .write_all(encode_line(&msg).as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        // Shared response router: agent→client requests (e.g. permission
        // prompts) register a oneshot here; client responses are routed back.
        let pending: Arc<Mutex<HashMap<Id, oneshot::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let idgen = Arc::new(AtomicU64::new(1));

        let mut lines = BufReader::new(self.reader).lines();
        let agent = self.agent;
        while let Ok(Some(line)) = lines.next_line().await {
            let msg = match decode_line(&line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            match msg {
                // Run each request on its own task so the read loop stays free
                // to route the client's responses (e.g. a permission decision)
                // back to an in-flight agent call such as `session/prompt`.
                Message::Request { id, method, params } => {
                    let agent = agent.clone();
                    let write_tx = write_tx.clone();
                    let pending = pending.clone();
                    let idgen = idgen.clone();
                    tokio::spawn(async move {
                        let resp = dispatch(
                            agent.as_ref(),
                            &write_tx,
                            &pending,
                            &idgen,
                            id,
                            &method,
                            params,
                        )
                        .await;
                        let _ = write_tx.send(resp);
                    });
                }
                Message::Notification { params, .. } => {
                    agent.handle_notification(params).await;
                }
                Message::Response { id, result } => {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(Message::Response { id, result });
                    }
                }
            }
        }
        drop(write_tx);
        let _ = write_task.await;
    }
}

async fn dispatch<A: AcpAgent + ?Sized>(
    agent: &A,
    sender: &mpsc::UnboundedSender<Message>,
    pending: &Arc<Mutex<HashMap<Id, oneshot::Sender<Message>>>>,
    idgen: &Arc<AtomicU64>,
    id: Id,
    method: &str,
    params: Value,
) -> Message {
    let result = if method.starts_with('_') {
        // Custom extension methods (e.g. `_agentgrid/nodes`); the gateway
        // exposes read-only control-plane views to external ACP clients.
        agent.handle_extension(method, params).await
    } else {
        match method {
            "initialize" => match from_value::<InitializeParams>(params) {
                Ok(p) => agent.initialize(p).await,
                Err(e) => Err(e),
            },
            "session/new" => match from_value::<SessionNewParams>(params) {
                Ok(p) => agent.session_new(p).await,
                Err(e) => Err(e),
            },
            "session/prompt" => match serde_json::from_value::<SessionPromptParams>(params) {
                Ok(p) => {
                    agent
                        .session_prompt(
                            AcpCtx {
                                sender: sender.clone(),
                                pending: pending.clone(),
                                idgen: idgen.clone(),
                            },
                            p,
                        )
                        .await
                }
                Err(e) => Err(RpcError {
                    code: -32602,
                    message: format!("invalid params: {e}"),
                    data: None,
                }),
            },
            "session/cancel" => match from_value::<SessionCancelParams>(params) {
                Ok(p) => agent.session_cancel(p).await,
                Err(e) => Err(e),
            },
            _ => Err(RpcError {
                code: -32601,
                message: "method not found".into(),
                data: None,
            }),
        }
    };
    Message::Response { id, result }
}

fn from_value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError {
        code: -32602,
        message: format!("invalid params: {e}"),
        data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::new;
    use crate::methods::map_session_update;
    use agentgrid_common::EventKind;
    use std::sync::Arc;
    use tokio::io::duplex;

    struct MockAcpAgent;

    impl AcpAgent for MockAcpAgent {
        async fn initialize(&self, _p: InitializeParams) -> Result<Value, RpcError> {
            Ok(serde_json::json!({ "protocol_version": "0.1", "capabilities": {}, "client": {} }))
        }
        async fn session_new(&self, _p: SessionNewParams) -> Result<Value, RpcError> {
            Ok(serde_json::json!({ "session_id": "sess-1" }))
        }
        async fn session_prompt(
            &self,
            ctx: AcpCtx,
            _p: SessionPromptParams,
        ) -> Result<Value, RpcError> {
            notify_update(
                &ctx.sender,
                "sess-1",
                serde_json::json!({ "type": "progress", "text": "working" }),
            );
            notify_update(
                &ctx.sender,
                "sess-1",
                serde_json::json!({ "type": "tool_call", "tool": "bash" }),
            );
            Ok(serde_json::json!({ "status": "done" }))
        }
        async fn session_cancel(&self, _p: SessionCancelParams) -> Result<Value, RpcError> {
            Ok(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn acp_server_drives_lifecycle_over_pipe() {
        let (c2s, s_read) = duplex(4096);
        let (s_write, c_read) = duplex(4096);
        tokio::spawn(AcpServer::new(s_read, s_write, MockAcpAgent).run());

        let (client, mut notif) = new(c_read, c2s);
        let client = Arc::new(client);

        let init = client
            .initialize(InitializeParams {
                protocol_version: "0.1".into(),
                agent: "grid".into(),
                model: "v1".into(),
                session_id: None,
                cwd: "/tmp".into(),
                capabilities: serde_json::json!({}),
                client: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(init.protocol_version, "0.1");

        let new_res = client
            .session_new(SessionNewParams {
                agent: "grid".into(),
                model: None,
                cwd: "/tmp".into(),
                prompt: None,
                mcp: Value::Null,
                parent_session_id: None,
            })
            .await
            .unwrap();
        assert_eq!(new_res.session_id, "sess-1");

        let prompt = tokio::spawn({
            let c = client.clone();
            async move {
                c.session_prompt(SessionPromptParams {
                    session_id: "sess-1".into(),
                    prompt: "do it".into(),
                })
                .await
            }
        });

        let mut kinds = Vec::new();
        for _ in 0..2 {
            if let Message::Notification { params, .. } = notif.recv().await.unwrap() {
                let sid = params
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let env = map_session_update(sid, params.get("update").unwrap());
                kinds.push(env.kind);
            }
        }
        assert_eq!(kinds, vec![EventKind::Progress, EventKind::ToolCall]);

        let res = prompt.await.unwrap().unwrap();
        assert_eq!(res.get("status").unwrap(), &serde_json::json!("done"));
    }
}
