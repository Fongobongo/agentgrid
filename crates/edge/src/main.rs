//! `agentgrid-cp-edge` — a standalone forwarder in front of an HA cluster.
//!
//! The forwarding itself lives in the crate's library, because the node
//! daemon hosts the same code as a peer relay (`AGENTGRID_RELAY_LISTEN`).
//! This binary only parses flags and builds an [`Upstream::Ha`]: leader
//! discovery via `/v1/ha/status` with the cluster token.
//!
//! Use this when the middle host is not an agentgrid node. When it is a node,
//! `AGENTGRID_RELAY_LISTEN` on that node is enough — no second binary, no
//! cluster token, and the relay follows the daemon's current control-plane
//! endpoint for free.

use std::time::Duration;

use clap::Parser;

use agentgrid_edge::{serve, Edge, EdgeConfig, HaUpstream, Upstream};

#[derive(Parser)]
#[command(name = "agentgrid-cp-edge", version, about)]
struct Args {
    /// Listen address, e.g. 0.0.0.0:7900.
    #[arg(long, env = "AGENTGRID_EDGE_LISTEN", default_value = "0.0.0.0:7900")]
    listen: String,
    /// Comma-separated control-plane base URLs.
    #[arg(long, env = "AGENTGRID_EDGE_UPSTREAMS")]
    upstreams: String,
    /// HA cluster token (also AGENTGRID_HA_CLUSTER_TOKEN).
    #[arg(long, env = "AGENTGRID_HA_CLUSTER_TOKEN")]
    cluster_token: String,
    /// Seconds to wait for an upstream to start answering. Bounds only the
    /// response head, so SSE and artifact downloads keep streaming. A leader
    /// that stops answering (partition, SIGSTOP, long GC pause) must become a
    /// retry on the new leader, not a client request that hangs forever.
    #[arg(
        long,
        env = "AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS",
        default_value_t = 10
    )]
    upstream_timeout_secs: u64,
    /// Comma-separated CIDR prefixes allowed to use this edge. Empty = any
    /// source, which is the historical behaviour: a standalone edge is
    /// expected to sit behind the operator's own network policy.
    #[arg(long, env = "AGENTGRID_EDGE_ALLOW")]
    allow: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let upstreams: Vec<String> = args
        .upstreams
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .collect();
    let allowed_sources = agentgrid_edge::parse_allowlist(args.allow.as_deref())?;
    let edge = Edge::new(EdgeConfig {
        upstream: Upstream::Ha(HaUpstream {
            upstreams,
            token: args.cluster_token,
        }),
        upstream_timeout: Duration::from_secs(args.upstream_timeout_secs.max(1)),
        allowed_sources,
    })?;
    let listen: std::net::SocketAddr = args.listen.parse()?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    serve(edge, listener).await
}
