//! `agentgrid acp-agent`: run Agentgrid as an ACP *agent* over stdio so an
//! external ACP client can drive tasks on the control plane. Stage 6 northbound
//! gateway entry point.
//!
//! Env: `AGENTGRID_SERVER` (control plane base URL), `AGENTGRID_TOKEN`
//! (optional bearer token for task creation).

use agentgrid_acp::gateway::GatewayAgent;
use agentgrid_acp::server::AcpServer;
use tokio::io::{stdin, stdout};

#[tokio::main]
async fn main() {
    let server =
        std::env::var("AGENTGRID_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7800".into());
    let token = std::env::var("AGENTGRID_TOKEN").ok();
    let agent = GatewayAgent::new(server, token);
    AcpServer::new(stdin(), stdout(), agent).run().await;
}
