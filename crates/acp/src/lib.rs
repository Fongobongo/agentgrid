//! Stage 5 — ACP southbound client + Stage 6 northbound server.
//!
//! Pure JSON-RPC 2.0 codec + stdio-style transport + ACP method DTOs +
//! `session/update` → `AgentEventEnvelope` mapping + the durable approval
//! state machine + an `AcpServer` that lets a Rust process speak the *agent*
//! side of ACP (foundation for the `agentgrid acp-agent` subcommand).

pub mod approval;
pub mod client;
pub mod codec;
pub mod gateway;
pub mod methods;
pub mod server;

pub use approval::{next_approval, ApprovalEvent, ApprovalStatus, InvalidApprovalTransition};
pub use client::{new, AcpClient, AcpError};
pub use codec::{decode_line, encode_line, CodecError, Id, Message, RpcError};
pub use gateway::GatewayAgent;
pub use methods::{
    map_session_update, session_new_request, session_update_message, update_type_to_kind,
    InitializeParams, InitializeResult, SessionCancelParams, SessionNewParams, SessionNewResult,
    SessionPromptParams, SessionRequestPermissionParams,
};
pub use server::{notify_update, AcpAgent, AcpCtx, AcpServer};
