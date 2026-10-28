//! `ag cluster status|promote|demote|snapshot` — HA operator commands.
//!
//! These talk to the peer RPC surface and authenticate with the cluster token
//! (`--token` or `AGENTGRID_HA_CLUSTER_TOKEN`), not the user session JWT.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::err_if_fail;
const TOKEN_HEADER: &str = "x-agentgrid-ha-token";

#[derive(Args)]
pub struct ClusterArgs {
    /// Cluster token (also AGENTGRID_HA_CLUSTER_TOKEN).
    #[arg(long, env = "AGENTGRID_HA_CLUSTER_TOKEN", global = true)]
    pub token: String,
    #[command(subcommand)]
    pub command: ClusterCmd,
}

#[derive(Subcommand)]
pub enum ClusterCmd {
    /// Show this replica's HA role, term, epoch and known leader.
    Status,
    /// Promote this data replica to leader (manual failover).
    Promote,
    /// Step this replica down from leader.
    Demote,
    /// Ask the leader to write a snapshot now.
    Snapshot,
    /// Fetch the shared JWT secret from the leader (provisioning).
    FetchSecret {
        /// Write the secret to this file with mode 0600 instead of stdout.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

pub async fn cmd_cluster(
    client: &reqwest::Client,
    base: &str,
    json: bool,
    a: ClusterArgs,
) -> Result<()> {
    match a.command {
        ClusterCmd::Status => {
            let out = call(
                client,
                base,
                &a.token,
                reqwest::Method::GET,
                "/v1/ha/status",
                "status",
            )
            .await?;
            print_status(&out, json);
        }
        ClusterCmd::Promote => {
            let out = call(
                client,
                base,
                &a.token,
                reqwest::Method::POST,
                "/v1/ha/promote",
                "promote",
            )
            .await?;
            print_result(&out, json, "promoted");
        }
        ClusterCmd::Demote => {
            let out = call(
                client,
                base,
                &a.token,
                reqwest::Method::POST,
                "/v1/ha/demote",
                "demote",
            )
            .await?;
            print_result(&out, json, "demoted");
        }
        ClusterCmd::Snapshot => {
            let out = call(
                client,
                base,
                &a.token,
                reqwest::Method::POST,
                "/v1/ha/snapshot",
                "snapshot",
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "snapshot {} (seq {}, epoch {}, {} bytes)",
                    out.get("snapshot_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    out.get("seq").and_then(Value::as_u64).unwrap_or(0),
                    out.get("epoch").and_then(Value::as_u64).unwrap_or(0),
                    out.get("size").and_then(Value::as_u64).unwrap_or(0),
                );
            }
        }
        ClusterCmd::FetchSecret { out } => {
            let value = call(
                client,
                base,
                &a.token,
                reqwest::Method::GET,
                "/v1/ha/secret",
                "fetch-secret",
            )
            .await?;
            let secret = value
                .get("secret")
                .and_then(Value::as_str)
                .context("fetch-secret response has no secret")?;
            let fingerprint = value
                .get("fingerprint")
                .and_then(Value::as_str)
                .unwrap_or("-");
            match out {
                Some(path) => {
                    std::fs::write(&path, format!("{secret}\n"))
                        .with_context(|| format!("write {}", path.display()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                            .with_context(|| format!("chmod {}", path.display()))?;
                    }
                    println!(
                        "wrote JWT secret to {} (fingerprint {fingerprint})",
                        path.display()
                    );
                }
                None => {
                    println!("{secret}");
                    eprintln!("fingerprint={fingerprint}");
                }
            }
        }
    }
    Ok(())
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    what: &str,
) -> Result<Value> {
    let resp = client
        .request(method, format!("{base}{path}"))
        .header(TOKEN_HEADER, token)
        .send()
        .await
        .with_context(|| format!("{what} request failed"))?;
    err_if_fail(resp.status(), what)?;
    resp.json()
        .await
        .with_context(|| format!("parse {what} response"))
}

fn print_status(out: &Value, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    let s = |k: &str| {
        out.get(k)
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string()
    };
    let n = |k: &str| out.get(k).and_then(Value::as_u64).unwrap_or(0);
    println!(
        "node={} role={} term={} epoch={} applied_seq={} leader={}",
        s("node_id"),
        s("role"),
        n("term"),
        n("epoch"),
        n("applied_seq"),
        s("leader_id"),
    );
    if let Some(err) = out.get("error").and_then(Value::as_str) {
        println!("error={err}");
    }
}

fn print_result(out: &Value, json: bool, verb: &str) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!(
        "{verb}: role={} epoch={}",
        out.get("role").and_then(Value::as_str).unwrap_or("-"),
        out.get("epoch").and_then(Value::as_u64).unwrap_or(0),
    );
}
