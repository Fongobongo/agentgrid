//! Plan 0.3 2.2: the node WebSocket control channel (`/v1/node/ws`,
//! ADR 0009). WS carries only control messages — assignment push, ack,
//! cancel, heartbeat; the data plane stays on the HTTP endpoints. One
//! connection per node (a newer connection supersedes the older one with
//! close code 4003); assignments are pushed by a pump task that wakes on the
//! same `assignment_notify` the long-poll handlers wait on, so WS and poll
//! keep identical scheduling semantics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_common::ws::{
    NodeWsMsg, WS_CLOSE_BAD_PROTOCOL, WS_CLOSE_SUPERSEDED, WS_CLOSE_UNAUTHORIZED,
};
use agentgrid_common::PollRequest;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::AppState;

/// Server ping cadence; a node silent for 3 intervals is presumed gone
/// (the TCP close frees its registry slot).
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Time allowed for the node to send `hello` after the upgrade.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Safety-net pump cadence: even if a notify is missed, a queued task reaches
/// a connected WS node within one tick.
const PUMP_TICK: Duration = Duration::from_secs(1);
/// Outbound channel depth per connection.
const CHANNEL_CAPACITY: usize = 64;

static WS_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Cumulative assignment batches pushed over WS (plan 2.5 metrics).
static WS_PUSHES: AtomicU64 = AtomicU64::new(0);
static WS_REDELIVERIES: AtomicU64 = AtomicU64::new(0);

struct WsConn {
    gen: u64,
    tx: mpsc::Sender<Message>,
    max_concurrency: u32,
    /// Protocol 2+: the connection understands `AdapterEnvUpdate`. A
    /// protocol-1 node would fail to deserialize it and drop the session, so
    /// the flag gates the push rather than the message shape.
    adapter_env_capable: bool,
}

/// In-memory registry of connected WS nodes (ADR 0009). Lives in `AppState`;
/// the pump task and the cancel route push through it.
#[derive(Default)]
pub struct WsRegistry {
    conns: tokio::sync::Mutex<HashMap<String, WsConn>>,
}

impl WsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection; a previous connection of the same node is
    /// closed with code 4003 (superseded). Returns the new generation id so
    /// the connection can deregister itself on close without racing a newer
    /// registration.
    async fn register(
        &self,
        node_id: &str,
        tx: mpsc::Sender<Message>,
        max_concurrency: u32,
        adapter_env_capable: bool,
    ) -> u64 {
        let gen = WS_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        let mut conns = self.conns.lock().await;
        if let Some(old) = conns.insert(
            node_id.to_string(),
            WsConn {
                gen,
                tx,
                max_concurrency,
                adapter_env_capable,
            },
        ) {
            let _ = old.tx.try_send(Message::Close(Some(CloseFrame {
                code: WS_CLOSE_SUPERSEDED,
                reason: "superseded by a newer connection".into(),
            })));
        }
        gen
    }

    async fn remove(&self, node_id: &str, gen: u64) {
        let mut conns = self.conns.lock().await;
        if conns.get(node_id).is_some_and(|c| c.gen == gen) {
            conns.remove(node_id);
        }
    }

    /// Send a control message to a connected node. Best-effort: a full
    /// outbound channel drops the message and logs a warning; the 1s pump
    /// tick + reconnect pull covers the occasional drop.
    pub async fn send(&self, node_id: &str, msg: &NodeWsMsg) {
        if let Ok(text) = serde_json::to_string(msg) {
            let conns = self.conns.lock().await;
            if let Some(c) = conns.get(node_id) {
                if c.tx.try_send(Message::Text(text.into())).is_err() {
                    tracing::warn!(node_id, "ws outbound channel full; dropping message");
                }
            }
        }
    }

    /// Push the current sealed adapter env to every connected node that can
    /// read it.
    ///
    /// Called after an adapter-env write so a value set while a node is
    /// connected lands now rather than at its next poll — which, on this
    /// transport, would be never. Best-effort like [`Self::send`]: a missed
    /// push is repaired by the next connect (the CP sends it after `hello_ok`).
    pub async fn push_adapter_env(&self, store: &crate::store::Store) {
        let targets: Vec<String> = {
            let conns = self.conns.lock().await;
            conns
                .iter()
                .filter(|(_, c)| c.adapter_env_capable)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for node_id in targets {
            match store.adapter_env_for(&node_id).await {
                Ok(adapter_env) => {
                    self.send(
                        &node_id,
                        &NodeWsMsg::AdapterEnvUpdate {
                            adapter_env,
                            adapter_env_complete: true,
                        },
                    )
                    .await;
                }
                Err(e) => tracing::warn!(node_id = %node_id, "ws adapter env push failed: {e}"),
            }
        }
    }

    /// Number of connected WS nodes (surfaced in /metrics).
    pub async fn connection_count(&self) -> usize {
        self.conns.lock().await.len()
    }

    /// Close every connection with `code` (HA demotion: nodes must reconnect
    /// to the new leader instead of waiting for a ping timeout).
    pub async fn close_all(&self, code: u16, reason: &str) {
        let conns = self.conns.lock().await;
        for c in conns.values() {
            let _ = c.tx.try_send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.to_string().into(),
            })));
        }
    }
}

/// Reconnect redeliveries of unacked assignments (observability for the
/// pull added in handle_conn).
pub fn ws_redeliveries() -> u64 {
    WS_REDELIVERIES.load(Ordering::Relaxed)
}

pub fn ws_pushes() -> u64 {
    WS_PUSHES.load(Ordering::Relaxed)
}

/// Bearer node-credential auth BEFORE the upgrade (ADR 0009): an invalid or
/// revoked credential gets a plain HTTP 401, never a socket.
pub async fn node_ws(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let cred = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let node_id = match cred {
        Some(c) => match state.store.node_id_for_credential(c).await {
            Ok(Some(id)) => id,
            _ => return StatusCode::UNAUTHORIZED.into_response(),
        },
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    ws.on_upgrade(move |socket| handle_conn(state, node_id, socket))
}

async fn handle_conn(state: Arc<AppState>, node_id: String, socket: WebSocket) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(CHANNEL_CAPACITY);

    // Await `hello` before registering: the CP pushes nothing until the node
    // identified itself (protocol table in docs/node-ws-protocol.md).
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, ws_stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<NodeWsMsg>(&t).ok(),
        _ => None,
    };
    let Some(NodeWsMsg::Hello {
        node_id: hello_node,
        protocol_version,
        name,
        adapters,
        repositories,
        max_concurrency,
        ..
    }) = hello
    else {
        let _ = ws_sink
            .send(Message::Close(Some(CloseFrame {
                code: 1002,
                reason: "expected hello".into(),
            })))
            .await;
        return;
    };
    // The authenticated identity wins; a hello for a different node is a
    // protocol violation, not an identity switch.
    if hello_node != node_id {
        let _ = ws_sink
            .send(Message::Close(Some(CloseFrame {
                code: WS_CLOSE_UNAUTHORIZED,
                reason: "hello node_id does not match credential".into(),
            })))
            .await;
        return;
    }
    // ADR 0028: `sealed` in an adapter-env entry is protocol 2. A protocol-1
    // node would fail to deserialize the message and drop the session, so it
    // is never sent one. Computed here because `protocol_version` moves into
    // the registration below.
    let adapter_env_capable = protocol_version
        .as_deref()
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        >= 2;
    if agentgrid_common::is_incompatible_protocol(&protocol_version) {
        if let Err(e) = state.store.set_node_degraded(&node_id).await {
            tracing::warn!(node_id, "set_node_degraded failed: {e}");
        }
        let _ = ws_sink
            .send(Message::Close(Some(CloseFrame {
                code: WS_CLOSE_BAD_PROTOCOL,
                reason: "incompatible protocol version".into(),
            })))
            .await;
        return;
    }

    // A live WS socket is the strongest liveness signal there is, so make the
    // node schedulable *now* instead of waiting for its next heartbeat.
    //
    // `node_ineligibility` requires `status = online`, and that column is
    // refreshed only by `heartbeat` — or by `register_or_touch_node` on the
    // poll path. The WS path did neither, so after a failover a node that had
    // already connected to the new leader stayed invisible to the scheduler
    // until a heartbeat reached *that* leader. With the old leader frozen its
    // beats hung, so `run-ha-partition.sh` showed a 43 s hole: WS connected at
    // t+46 s, first attempt at t+89 s. Same call the poll transport makes, with
    // the capabilities the hello already carries.
    let registration = PollRequest {
        node_id: node_id.clone(),
        name,
        adapters,
        repositories,
        max_concurrency,
        protocol_version,
    };
    if let Err(e) = state.store.register_or_touch_node(&registration).await {
        // Registration is bookkeeping; the socket still works (unacked
        // redelivery below is keyed by node id, not by this row).
        tracing::warn!(node_id, "ws hello: node registration failed: {e}");
    }

    let gen = state
        .ws_registry
        .register(&node_id, tx, max_concurrency, adapter_env_capable)
        .await;
    let ok = NodeWsMsg::HelloOk {
        server_time: chrono::Utc::now().timestamp_millis(),
    };
    let _ = state.ws_registry.send(&node_id, &ok).await; // via channel

    // Config on connect. `adapter_env` used to travel only in a poll response,
    // and a node on this transport never polls — so on `ws`/`auto` the whole
    // feature silently did nothing.
    if adapter_env_capable {
        match state.store.adapter_env_for(&node_id).await {
            Ok(adapter_env) => {
                let _ = state
                    .ws_registry
                    .send(
                        &node_id,
                        &NodeWsMsg::AdapterEnvUpdate {
                            adapter_env,
                            adapter_env_complete: true,
                        },
                    )
                    .await;
            }
            Err(e) => tracing::warn!(node_id = %node_id, "ws adapter env on connect failed: {e}"),
        }
    }

    // A freshly connected node may already have free slots and queued work.
    // Reconnect pull for in-flight assignments: an assignment pushed to a
    // connection that died mid-delivery leaves the attempt `assigned`
    // (never acked) until the ack deadline — and the pump below only hands
    // out still-QUEUED work. Redeliver the node's unacked attempts over
    // the fresh socket so a flapping connection cannot strand work for a
    // full lease window (root cause of the ws_node_survives_cp_restart
    // flake).
    match state.store.unacked_assignments(&node_id).await {
        Ok(pending) if !pending.is_empty() => {
            WS_REDELIVERIES.fetch_add(1, Ordering::Relaxed);
            state
                .ws_registry
                .send(
                    &node_id,
                    &NodeWsMsg::Assignment {
                        assignments: pending,
                    },
                )
                .await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(node_id, "unacked assignment pull failed: {e}"),
    }
    state.assignment_notify.notify_waiters();

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // first tick completes immediately
    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if ws_sink.send(msg).await.is_err() {
                    break;
                }
            }
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        match serde_json::from_str::<NodeWsMsg>(&t) {
                            Ok(m) => handle_client_msg(&state, &node_id, m).await,
                            Err(e) => tracing::warn!(node_id, "ws parse error: {e}"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::debug!(node_id, "ws stream error: {e}");
                        break;
                    }
                    // Ping/Pong are answered by the ws layer; ignore Binary.
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if ws_sink.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }
    state.ws_registry.remove(&node_id, gen).await;
}

async fn handle_client_msg(state: &Arc<AppState>, node_id: &str, msg: NodeWsMsg) {
    match msg {
        NodeWsMsg::Ack {
            attempt_ids,
            fencing_tokens,
            ok,
            error,
        } => {
            for (i, id) in attempt_ids.iter().enumerate() {
                match state.store.attempt_owner(id).await {
                    Ok(Some(owner)) if owner == *node_id => {}
                    Ok(_) => {
                        tracing::warn!(node_id, attempt = %id, "ws ack for foreign attempt");
                        continue;
                    }
                    _ => continue,
                }
                // Plan 0.3 2.4: fencing applies on the WS path too — a stale
                // session must not ack an attempt that was reassigned/lost.
                if let Err(code) = crate::auth::check_fencing_token(
                    state,
                    id,
                    fencing_tokens.get(i).map(|s| s.as_str()),
                )
                .await
                {
                    tracing::warn!(node_id, attempt = %id, %code, "ws ack rejected: fencing token mismatch");
                    continue;
                }
                if ok {
                    if let Err(e) = state.store.ack_attempt(id).await {
                        tracing::error!(attempt = %id, "ws ack_attempt failed: {e}");
                    }
                } else {
                    // Rejected on the node: the attempt dies terminal (lost)
                    // and the task fails — legal NodeLost transitions from
                    // `assigned`. The old complete_attempt/Fail call was an
                    // invalid transition the handler swallowed, leaving the
                    // attempt assigned until the 30s reaper and cycling.
                    let reason = format!(
                        "node_rejected{}",
                        error
                            .as_deref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    );
                    if let Err(e) = state.store.reject_assignment(id, &reason).await {
                        tracing::warn!(attempt = %id, "ws reject_assignment failed: {e}");
                    }
                }
            }
            // Slots changed: let the pump try to fill them.
            state.assignment_notify.notify_waiters();
        }
        // Cancel is authoritative via the attempt status in the store; the
        // ack only confirms delivery (metrics in 2.5).
        NodeWsMsg::CancelAck { .. } => {}
        NodeWsMsg::Heartbeat { .. } => {
            state.assignment_notify.notify_waiters();
        }
        _ => tracing::warn!(node_id, "unexpected ws message direction"),
    }
}

/// Background pump: on every scheduler wake (task created, workflow spawn,
/// ack/heartbeat) push fresh assignments to all connected WS nodes. A 1 s
/// safety tick covers a missed notify. Polling nodes are unaffected — the
/// pump only talks to the registry.
pub fn start_pump(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            // Standbys have no API mutations and therefore no assignments to
            // push; idle until promotion.
            if !state.ha.is_leader() {
                tokio::time::sleep(PUMP_TICK).await;
                continue;
            }
            // Build the waiter BEFORE running the pass so a notify landing
            // mid-pass re-runs the loop. `enable()` registers this waiter
            // immediately: `notify_waiters` does NOT store a permit, so an
            // unpolled `Notified` misses a wake-up that lands while the pass
            // is running (the pump then sat on the 1 s safety tick — seen as
            // a flaky >200 ms push in ws_assignment_pushed_under_200ms).
            let notified = state.assignment_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            pump_once(&state).await;
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(PUMP_TICK) => {}
            }
        }
    });
}

async fn pump_once(state: &Arc<AppState>) {
    let targets: Vec<(String, u32)> = {
        let conns = state.ws_registry.conns.lock().await;
        conns
            .iter()
            .map(|(id, c)| (id.clone(), c.max_concurrency))
            .collect()
    };
    for (node_id, max_conc) in targets {
        // try_assign_batch caps at the node's free slots itself (store is the
        // source of truth); an empty result means no matching queued work.
        let cap = max_conc.max(1) as usize;
        match state.store.try_assign_batch(&node_id, cap).await {
            Ok(batch) if !batch.is_empty() => {
                WS_PUSHES.fetch_add(1, Ordering::Relaxed);
                state
                    .ws_registry
                    .send(&node_id, &NodeWsMsg::Assignment { assignments: batch })
                    .await;
            }
            Ok(_) => {}
            Err(e) => tracing::error!(node_id, "ws pump assign failed: {e}"),
        }
    }
}

/// Push `cancel` to the node holding a freshly cancel-requested attempt
/// (best-effort; the poll-based cancel probe remains authoritative for
/// non-WS nodes and as a fallback).
pub async fn push_cancel_for_task(state: &AppState, task_id: &str) {
    let targets = match state.store.cancel_targets_for_task(task_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("cancel_targets_for_task failed: {e}");
            return;
        }
    };
    for (attempt_id, node_id) in targets {
        state
            .ws_registry
            .send(&node_id, &NodeWsMsg::Cancel { attempt_id })
            .await;
    }
}
