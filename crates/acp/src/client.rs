//! ACP southbound client: JSON-RPC 2.0 over a bidirectional byte transport
//! (stdio in production, an in-memory pipe in tests). Owns a reader task that
//! routes responses back to the awaiting `request` by id and forwards
//! notifications (`session/update`, `session/request_permission`) to a channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use serde_json::Value;

use crate::codec::{CodecError, Id, Message, RpcError};
use crate::methods::*;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("rpc error: {0:?}")]
    Rpc(RpcError),
    #[error("notification channel closed")]
    ChannelClosed,
    #[error("unexpected message shape")]
    Unexpected,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

type PendingMap = Arc<Mutex<HashMap<Id, oneshot::Sender<Message>>>>;

pub struct AcpClient<R, W> {
    next: std::sync::atomic::AtomicU64,
    pending: PendingMap,
    writer: Arc<tokio::sync::Mutex<W>>,
    _reader: JoinHandle<()>,
    _marker: std::marker::PhantomData<R>,
}

/// Create a client over a byte transport. Returns the client and the
/// notification receiver (`session/update`, `session/request_permission`).
pub fn new<R, W>(reader: R, writer: W) -> (AcpClient<R, W>, mpsc::UnboundedReceiver<Message>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (notif_tx, notif_rx) = mpsc::unbounded_channel();
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let reader_task = {
        let pending = pending.clone();
        let notif_tx = notif_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(n) if n > 0 => {}
                    _ => break, // EOF or read error: transport closed
                };
                let line = buf.trim_end_matches(['\n', '\r']);
                let msg = match crate::codec::decode_line(line) {
                    Ok(m) => m,
                    Err(_) => continue, // skip malformed frames
                };
                match msg {
                    Message::Response { ref id, .. } => {
                        if let Some(tx) = pending.lock().unwrap().remove(id) {
                            let _ = tx.send(msg);
                        }
                    }
                    // Notifications and agent→client requests both flow to the
                    // caller (the node decides allow/deny on request_permission).
                    Message::Notification { .. } | Message::Request { .. } => {
                        let _ = notif_tx.send(msg);
                    }
                }
            }
            // Transport closed: fail any in-flight requests so they don't hang.
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Message::Response {
                    id: Id::Null,
                    result: Err(RpcError {
                        code: -32000,
                        message: "transport closed".into(),
                        data: None,
                    }),
                });
            }
        })
    };

    let client = AcpClient {
        next: std::sync::atomic::AtomicU64::new(1),
        pending,
        writer,
        _reader: reader_task,
        _marker: std::marker::PhantomData,
    };
    (client, notif_rx)
}

impl<R, W> AcpClient<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&self, msg: Message) -> Result<(), AcpError> {
        let mut w = self.writer.lock().await;
        w.write_all(crate::codec::encode_line(&msg).as_bytes())
            .await?;
        w.flush().await?;
        Ok(())
    }

    /// Send a request and await its matching response (matched by id).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = Id::Num(self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as i64);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        self.send(Message::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        })
        .await?;
        let msg = rx.await.map_err(|_| AcpError::ChannelClosed)?;
        match msg {
            Message::Response { result, .. } => match result {
                Ok(v) => Ok(v),
                Err(e) => Err(AcpError::Rpc(e)),
            },
            _ => Err(AcpError::Unexpected),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        self.send(Message::Notification {
            method: method.to_string(),
            params,
        })
        .await
    }

    /// `initialize`: version/capability negotiation. Unknown optional
    /// capabilities arrive as extra `Value` fields and are tolerated.
    pub async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, AcpError> {
        let v = self
            .request(METHOD_INITIALIZE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn session_new(
        &self,
        params: SessionNewParams,
    ) -> Result<SessionNewResult, AcpError> {
        let v = self
            .request(METHOD_SESSION_NEW, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn session_prompt(&self, params: SessionPromptParams) -> Result<Value, AcpError> {
        self.request(METHOD_SESSION_PROMPT, serde_json::to_value(params)?)
            .await
    }

    pub async fn session_cancel(&self, params: SessionCancelParams) -> Result<Value, AcpError> {
        self.request(METHOD_SESSION_CANCEL, serde_json::to_value(params)?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::methods::map_session_update;
    use tokio::io::AsyncWriteExt;

    /// A minimal fake ACP agent: echoes `session/new` with a fixed session id
    /// and streams two `session/update` notifications, then answers prompt.
    async fn fake_agent(mut r: tokio::io::DuplexStream, mut w: tokio::io::DuplexStream) {
        let mut lines = BufReader::new(&mut r);
        let mut buf = String::new();
        while let Ok(n) = lines.read_line(&mut buf).await {
            if n == 0 {
                break;
            }
            let line = buf.trim_end_matches(['\n', '\r']);
            let msg = match crate::codec::decode_line(line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let Message::Request { id, method, .. } = msg {
                let resp = match method.as_str() {
                    METHOD_SESSION_NEW => Message::Response {
                        id,
                        result: Ok(serde_json::json!({ "session_id": "sess-1" })),
                    },
                    METHOD_SESSION_PROMPT => {
                        // emit updates then finish
                        let upd = |t: &str| {
                            session_update_message(
                                "sess-1",
                                serde_json::json!({ "type": t, "x": 1 }),
                            )
                        };
                        for u in [upd("plan"), upd("tool_call"), upd("result")] {
                            w.write_all(crate::codec::encode_line(&u).as_bytes())
                                .await
                                .unwrap();
                        }
                        Message::Response {
                            id,
                            result: Ok(serde_json::json!({ "status": "done" })),
                        }
                    }
                    METHOD_SESSION_CANCEL => Message::Response {
                        id,
                        result: Ok(serde_json::json!({})),
                    },
                    METHOD_INITIALIZE => Message::Response {
                        id,
                        result: Ok(serde_json::json!({
                            "protocol_version": "0.2",
                            "capabilities": { "extra_unknown_cap": true }
                        })),
                    },
                    _ => Message::Response {
                        id,
                        result: Err(RpcError {
                            code: -32601,
                            message: "method not found".into(),
                            data: None,
                        }),
                    },
                };
                w.write_all(crate::codec::encode_line(&resp).as_bytes())
                    .await
                    .unwrap();
                w.flush().await.unwrap();
            }
            buf.clear();
        }
    }

    #[tokio::test]
    async fn initialize_and_session_lifecycle_with_updates() {
        let (c2a, a_read) = tokio::io::duplex(4096);
        let (a_write, c_read) = tokio::io::duplex(4096);
        tokio::spawn(fake_agent(a_read, a_write));

        let (client, mut notif) = new(c_read, c2a);

        let init = client
            .initialize(InitializeParams {
                protocol_version: "0.2".into(),
                capabilities: serde_json::json!({}),
                client: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(init.protocol_version, "0.2");

        let new_res = client
            .session_new(SessionNewParams {
                agent: "opencode".into(),
                model: None,
                cwd: "/abs/work".into(),
                prompt: None,
                mcp: serde_json::Value::Null,
                parent_session_id: None,
            })
            .await
            .unwrap();
        assert_eq!(new_res.session_id, "sess-1");

        // Start a prompt; updates should arrive on the notification channel.
        let prompt_handle = tokio::spawn(async move {
            client
                .session_prompt(SessionPromptParams {
                    session_id: "sess-1".into(),
                    prompt: "do it".into(),
                })
                .await
        });

        let mut kinds = Vec::new();
        for _ in 0..3 {
            let m = notif.recv().await.unwrap();
            if let Message::Notification { params, .. } = m {
                let sid = params
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap()
                    .to_string();
                let upd = params.get("update").unwrap();
                let env = map_session_update(&sid, upd);
                kinds.push(env.kind);
            }
        }
        assert_eq!(
            kinds,
            vec![
                agentgrid_common::EventKind::Plan,
                agentgrid_common::EventKind::ToolCall,
                agentgrid_common::EventKind::Result,
            ]
        );

        let prompt_res = prompt_handle.await.unwrap().unwrap();
        assert_eq!(
            prompt_res.get("status").unwrap(),
            &serde_json::json!("done")
        );
    }

    #[tokio::test]
    async fn cancel_is_rpc_error_on_unknown_method() {
        let (c2a, a_read) = tokio::io::duplex(4096);
        let (a_write, c_read) = tokio::io::duplex(4096);
        tokio::spawn(fake_agent(a_read, a_write));
        let (client, _notif) = new(c_read, c2a);
        let err = client.request("bogus/method", serde_json::json!({})).await;
        assert!(matches!(err, Err(AcpError::Rpc(_))));
    }
}
