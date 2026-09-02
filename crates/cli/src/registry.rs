//! Registry/agent commands (`ag learn`, `ag agent`, `ag approvals`,
//! `ag skills`, `ag mcp`, `ag profiles`, `ag opencode`) — extracted from
//! main.rs in the CLI monolith split. Shared helpers live in main.rs.

use agentgrid_common::{AgentProfile, ApprovalView, SkillTrustView};
use anyhow::{Context, Result};

use crate::{err_if_fail, list_items};
use clap::{Args, Subcommand};

/// Plan 2.8 (#19): per-repo learning management.
#[derive(Args)]
pub(crate) struct LearnArgs {
    #[command(subcommand)]
    action: LearnAction,
}

#[derive(Subcommand)]
pub(crate) enum LearnAction {
    /// `ag learn list <repo> [--approved-only]`
    List {
        repo: String,
        #[arg(long = "approved-only", default_value_t = false)]
        approved_only: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// `ag learn add <repo> "<statement>" [--confidence 0.7] [--from-attempt <id>]`
    Add {
        repo: String,
        statement: String,
        #[arg(long, default_value_t = 0.5)]
        confidence: f64,
        #[arg(long = "from-attempt")]
        source_attempt_id: Option<String>,
    },
    /// `ag learn approve <id>` or `ag learn approve <id> --unapprove`
    Approve {
        id: String,
        #[arg(long = "unapprove", default_value_t = false)]
        unapprove: bool,
    },
    /// `ag learn remove <id>`
    Remove { id: String },
}

/// Plan 2.1 (#18): org-agent management.
#[derive(Args)]
pub(crate) struct AgentArgs {
    #[command(subcommand)]
    command: AgentSub,
}

#[derive(Subcommand)]
pub(crate) enum AgentSub {
    /// Register a long-lived org agent: `ag agent add <name> [--role R] [--prompt P] [--skills s1,s2] [--max-tasks N] [--budget-usd F] [--heartbeat SECS]`.
    Add {
        /// Unique path-safe name (used as the agent id key for references).
        name: String,
        /// Org role tag (display/org-chart only for now).
        #[arg(long)]
        role: Option<String>,
        /// Agent prompt template (used for heartbeat-spawned tasks; `{objective}` is not substituted yet).
        #[arg(long)]
        prompt: Option<String>,
        /// Comma-separated skill names to attach.
        #[arg(long)]
        skills: Option<String>,
        /// Hard-stop: max tasks this agent may spawn (NULL = unlimited).
        #[arg(long)]
        max_tasks: Option<i64>,
        /// Display budget in USD (no enforcement yet; max_tasks is the hard stop).
        #[arg(long)]
        budget_usd: Option<f64>,
        /// Heartbeat interval in seconds: spawn the prompt task on this cadence (NULL = no heartbeat).
        #[arg(long)]
        heartbeat: Option<i64>,
    },
    /// List org agents with current spend.
    List,
    /// Show one agent's immutable action trail.
    Actions { agent_id: String },
}

#[derive(Args)]
pub(crate) struct ApprovalArgs {
    #[command(subcommand)]
    action: ApprovalAction,
}

#[derive(Args)]
pub(crate) struct SkillsArgs {
    #[command(subcommand)]
    action: SkillsAction,
}

#[derive(Subcommand)]
pub(crate) enum SkillsAction {
    /// List recorded skill trust decisions.
    List {
        /// Filter by skill source tier: project|user|managed.
        #[arg(long)]
        source: Option<String>,
    },
    /// Trust a skill (allow the agent to load/execute it).
    Trust(SkillsNameArgs),
    /// Untrust a skill (fail-closed: the agent must not use it).
    Untrust(SkillsNameArgs),
    /// Plan 2.2 (#5): static security scan of a skill dir or SKILL.md file (dry-run).
    Scan {
        /// Path to a SKILL.md file or a skill directory to scan.
        path: String,
    },
}

#[derive(Args)]
pub(crate) struct McpArgs {
    #[command(subcommand)]
    action: McpAction,
}

#[derive(Subcommand)]
pub(crate) enum McpAction {
    /// List registered MCP servers.
    List,
    /// Register or replace an MCP server in the operator registry.
    Create {
        id: String,
        name: String,
        command: String,
        /// Args to pass (repeatable).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Env var names the server requires (repeatable; values resolved at spawn).
        #[arg(long = "env-requirement")]
        env_requirements: Vec<String>,
        /// Register as disabled (default enabled).
        #[arg(long)]
        disabled: bool,
    },
    /// Delete a server.
    Delete { id: String },
    /// Plan 2.2 (#5): scan a registered MCP server's command/args (dry-run).
    Scan { id: String },
}

#[derive(Args)]
pub(crate) struct SkillsNameArgs {
    /// Skill name (as discovered from SKILL.md frontmatter).
    name: String,
    /// Where the skill was found: project|user|managed (default project).
    #[arg(long, default_value = "project")]
    source: String,
}

#[derive(Args)]
pub(crate) struct ProfilesArgs {
    #[command(subcommand)]
    action: ProfilesAction,
}

#[derive(Subcommand)]
pub(crate) enum ProfilesAction {
    /// List profile ids that have an active revision.
    List,
    /// Show all revisions of a profile (newest first).
    Show { id: String },
    /// Create a new revision of a profile (does not activate it).
    Create(ProfileCreateArgs),
    /// Activate a specific revision (rollback = activate an older one).
    Activate { id: String, revision: i64 },
}

#[derive(Args)]
pub(crate) struct ProfileCreateArgs {
    id: String,
    /// System prompt text (inline). Empty string allowed.
    #[arg(long, default_value = "")]
    system_prompt: String,
    /// Autonomy level: l0|l1|l2|l3|l4 (default l2).
    #[arg(long, default_value = "l2")]
    autonomy: String,
    /// Max RSS in bytes.
    #[arg(long)]
    memory_max: Option<i64>,
    /// CPU quota, percent of one core (200 = 2 cores).
    #[arg(long)]
    cpu_quota: Option<i64>,
    /// Max tasks (PIDs).
    #[arg(long)]
    tasks_max: Option<i64>,
    /// Required secret env name (repeatable; names only, never values).
    #[arg(long = "secret-required", value_name = "ENV")]
    secret_required: Vec<String>,
    /// Optional secret env name (repeatable; warn-only if unset).
    #[arg(long = "secret-optional", value_name = "ENV")]
    secret_optional: Vec<String>,
    /// Adapter version this profile targets (SemVer; major must match).
    #[arg(long)]
    adapter_version: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum ApprovalAction {
    /// List approvals (optionally filter by status).
    List {
        /// Filter by status: pending|allowed|denied|expired|cancelled.
        status: Option<String>,
    },
    /// Allow a pending approval by id.
    Allow(ApprovalIdArgs),
    /// Deny a pending approval by id.
    Deny(ApprovalIdArgs),
}

#[derive(Args)]
pub(crate) struct ApprovalIdArgs {
    id: String,
    /// Optional reason recorded with the decision (audit trail).
    #[arg(long)]
    reason: Option<String>,
}

/// Plan 2.8 (#19): repo learnings CLI.
pub(crate) async fn cmd_learn(client: &reqwest::Client, base: &str, a: LearnArgs) -> Result<()> {
    match a.action {
        LearnAction::List {
            repo,
            approved_only,
            limit,
        } => {
            let url = if approved_only {
                format!("{base}/v1/repos/{repo}/learnings?approved=true&limit={limit}")
            } else {
                format!("{base}/v1/repos/{repo}/learnings?limit={limit}")
            };
            let r = client.get(&url).send().await.context("list learnings")?;
            if !r.status().is_success() {
                anyhow::bail!("list learnings failed ({})", r.status());
            }
            let rows: Vec<agentgrid_common::RepoLearning> =
                r.json().await.context("parse learnings response")?;
            if rows.is_empty() {
                println!("(no learnings for repo {repo})");
                return Ok(());
            }
            for r in rows {
                let flag = if r.approved { "A" } else { " " };
                let stop = r.source_attempt_id.as_deref().unwrap_or("");
                println!(
                    "{} {} [{:.2}] {} {}",
                    flag,
                    r.id,
                    r.confidence,
                    &stop[..8.min(stop.len())],
                    r.statement
                );
            }
            Ok(())
        }
        LearnAction::Add {
            repo,
            statement,
            confidence,
            source_attempt_id,
        } => {
            let body = serde_json::json!({
                "repository": repo, "statement": statement, "confidence": confidence,
                "source_attempt_id": source_attempt_id,
            });
            let r = client
                .post(format!("{base}/v1/repos/{repo}/learnings"))
                .json(&body)
                .send()
                .await
                .context("add learning")?;
            if !r.status().is_success() {
                anyhow::bail!("add learning failed ({})", r.status());
            }
            let row: agentgrid_common::RepoLearning = r.json().await.context("parse")?;
            println!("added learning {} (pending approval)", row.id);
            Ok(())
        }
        LearnAction::Approve { id, unapprove } => {
            let body = serde_json::json!({ "approved": !unapprove });
            let r = client
                .post(format!("{base}/v1/learnings/{id}/approve"))
                .json(&body)
                .send()
                .await
                .context("approve learning")?;
            match r.status() {
                s if s.is_success() => {
                    println!("learning {id} approved={}", !unapprove);
                    Ok(())
                }
                reqwest::StatusCode::NOT_FOUND => anyhow::bail!("learning {id} not found"),
                s => anyhow::bail!("approve learning failed ({s})"),
            }
        }
        LearnAction::Remove { id } => {
            let r = client
                .delete(format!("{base}/v1/learnings/{id}"))
                .send()
                .await
                .context("remove learning")?;
            match r.status() {
                s if s.is_success() => {
                    println!("learning {id} removed");
                    Ok(())
                }
                reqwest::StatusCode::NOT_FOUND => anyhow::bail!("learning {id} not found"),
                s => anyhow::bail!("remove learning failed ({s})"),
            }
        }
    }
}

/// Plan 2.1 (#18): org agents via the control-plane API.
pub(crate) async fn cmd_agent(client: &reqwest::Client, base: &str, a: AgentArgs) -> Result<()> {
    match a.command {
        AgentSub::Add {
            name,
            role,
            prompt,
            skills,
            max_tasks,
            budget_usd,
            heartbeat,
        } => {
            let body = serde_json::json!({
                "name": name,
                "role": role.unwrap_or_else(|| "worker".into()),
                "prompt": prompt.unwrap_or_default(),
                "skills": skills
                    .map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "budget_usd": budget_usd.unwrap_or(0.0),
                "max_tasks": max_tasks,
                "heartbeat_interval_secs": heartbeat,
            });
            let resp = client
                .post(format!("{base}/v1/agents"))
                .json(&body)
                .send()
                .await
                .context("create agent request failed")?;
            err_if_fail(resp.status(), "create agent")?;
            let agent: agentgrid_common::Agent =
                resp.json().await.context("parse agent response")?;
            println!("agent {} created (id {})", agent.name, agent.id);
            Ok(())
        }
        AgentSub::List => {
            let resp = client
                .get(format!("{base}/v1/agents"))
                .send()
                .await
                .context("list agents request failed")?;
            err_if_fail(resp.status(), "list agents")?;
            let agents: Vec<agentgrid_common::Agent> =
                resp.json().await.context("parse agents response")?;
            if agents.is_empty() {
                println!("(no agents registered)");
            }
            for a in &agents {
                let hb = a
                    .heartbeat_interval_secs
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "-".into());
                let spend = match a.max_tasks {
                    Some(max) => format!("{}/{max}", a.tasks_spent),
                    None => format!("{}", a.tasks_spent),
                };
                println!(
                    "{}  role={}  budget=${:.2}  tasks={spend}  heartbeat={hb}  (id {})",
                    a.name, a.role, a.budget_usd, a.id
                );
            }
            Ok(())
        }
        AgentSub::Actions { agent_id } => {
            let resp = client
                .get(format!("{base}/v1/agents/{agent_id}/actions"))
                .send()
                .await
                .context("agent actions request failed")?;
            err_if_fail(resp.status(), "agent actions")?;
            let actions: Vec<agentgrid_common::AgentAction> =
                resp.json().await.context("parse actions response")?;
            if actions.is_empty() {
                println!("(no actions recorded)");
            }
            for x in &actions {
                println!("{} [{}] {}", x.created_at, x.action, x.detail);
            }
            Ok(())
        }
    }
}

pub(crate) async fn cmd_approvals(
    client: &reqwest::Client,
    base: &str,
    a: ApprovalArgs,
) -> Result<()> {
    match a.action {
        ApprovalAction::List { status } => {
            let mut url = format!("{base}/v1/approvals");
            if let Some(s) = status {
                url.push_str(&format!("?status={s}"));
            }
            let resp = client
                .get(&url)
                .send()
                .await
                .context("approvals list request failed")?;
            err_if_fail(resp.status(), "approvals list")?;
            let v: serde_json::Value = resp.json().await.context("bad approvals json")?;
            let approvals: Vec<ApprovalView> =
                serde_json::from_value(serde_json::Value::Array(list_items(&v)))
                    .context("bad approvals json")?;
            for ap in &approvals {
                println!(
                    "{:<36} {:<10} {:<9} {}",
                    ap.id,
                    format!("{:?}", ap.status),
                    ap.task_id,
                    ap.permission
                );
            }
            Ok(())
        }
        ApprovalAction::Allow(id) => {
            answer_approval(client, base, &id.id, "allow", id.reason.as_deref()).await
        }
        ApprovalAction::Deny(id) => {
            answer_approval(client, base, &id.id, "deny", id.reason.as_deref()).await
        }
    }
}

async fn answer_approval(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    decision: &str,
    reason: Option<&str>,
) -> Result<()> {
    let body = reason
        .map(|r| serde_json::json!({ "reason": r }))
        .unwrap_or_else(|| serde_json::json!({}));
    let resp = client
        .post(format!("{base}/v1/approvals/{id}/{decision}"))
        .json(&body)
        .send()
        .await
        .context("approval answer request failed")?;
    err_if_fail(resp.status(), "approval answer")?;
    println!("approval {id} -> {decision}");
    Ok(())
}

/// Stage 9.2: skill trust management. A skill absent from the ledger is
/// `untrusted` (fail-closed); trust/untrust records the operator decision.
pub(crate) async fn cmd_skills(client: &reqwest::Client, base: &str, a: SkillsArgs) -> Result<()> {
    match a.action {
        SkillsAction::List { source } => {
            let mut url = format!("{base}/v1/skills");
            if let Some(s) = source {
                url.push_str(&format!("?source={s}"));
            }
            let resp = client
                .get(&url)
                .send()
                .await
                .context("skills list request failed")?;
            err_if_fail(resp.status(), "skills list")?;
            let rows: Vec<SkillTrustView> = resp.json().await.context("bad skills json")?;
            if rows.is_empty() {
                println!("no recorded skill trust decisions");
            }
            for s in &rows {
                println!(
                    "{:<24} {:<8} {:<8} {}",
                    s.name,
                    s.source,
                    if s.trusted { "trusted" } else { "untrusted" },
                    s.decided_by.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        SkillsAction::Trust(a) => set_skill_trust(client, base, &a.name, &a.source, "trust").await,
        SkillsAction::Untrust(a) => {
            set_skill_trust(client, base, &a.name, &a.source, "untrust").await
        }
        SkillsAction::Scan { path } => cmd_skill_scan(&path).await,
    }
}

/// Plan 2.2 (#5): `ag skill scan <path>` — dry-run static scan of a skill
/// file or directory. Walks `SKILL.md` files (or scans the given file
/// directly) and prints findings; exits 1 when any critical pattern trips.
pub(crate) async fn cmd_skill_scan(path: &str) -> Result<()> {
    use agentgrid_skills::scanner::{render_findings, scan_content};
    let p = std::path::Path::new(path);
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if p.is_dir() {
        let mut walk = std::collections::VecDeque::new();
        walk.push_back(p.to_path_buf());
        while let Some(dir) = walk.pop_front() {
            for entry in std::fs::read_dir(&dir).context("read dir failed")? {
                let e = entry.context("read entry failed")?;
                let ep = e.path();
                if ep.is_dir() {
                    walk.push_back(ep);
                } else if ep.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                    files.push(ep);
                }
            }
        }
    } else if p.is_file() {
        files.push(p.to_path_buf());
    } else {
        anyhow::bail!("path not found: {path}");
    }
    if files.is_empty() {
        anyhow::bail!("no SKILL.md files found under {path}");
    }
    let mut critical = 0usize;
    let mut total = 0usize;
    for f in &files {
        let content = std::fs::read_to_string(f).context("read skill failed")?;
        let findings = scan_content(&content);
        if findings.is_empty() {
            println!("{}: clean", f.display());
            continue;
        }
        total += findings.len();
        critical += findings
            .iter()
            .filter(|x| x.severity == agentgrid_skills::scanner::Severity::Critical)
            .count();
        println!("{}: {} finding(s)", f.display(), findings.len());
        print!("{}", render_findings(&findings));
    }
    if critical > 0 {
        anyhow::bail!("scan failed: {critical} critical finding(s) in {total} total");
    }
    if total > 0 {
        println!("scan complete: {total} warning(s), no critical findings");
    } else {
        println!("scan complete: clean");
    }
    Ok(())
}

pub(crate) async fn cmd_mcp(client: &reqwest::Client, base: &str, a: McpArgs) -> Result<()> {
    use agentgrid_common::{McpServer, McpServerCreate};
    match a.action {
        McpAction::List => {
            let resp = client
                .get(format!("{base}/v1/mcp-servers"))
                .send()
                .await
                .context("list mcp-servers request failed")?;
            err_if_fail(resp.status(), "list mcp-servers")?;
            let servers: Vec<McpServer> = resp.json().await.context("bad mcp json")?;
            if servers.is_empty() {
                println!("no MCP servers registered");
            }
            for s in &servers {
                println!(
                    "{:<12} {:<16} {:<16} {} args={} env=[{}]",
                    s.id,
                    s.name,
                    s.command,
                    if s.enabled { "[on]" } else { "[off]" },
                    s.args.len(),
                    s.env_requirements.join(",")
                );
            }
            Ok(())
        }
        McpAction::Create {
            id,
            name,
            command,
            args,
            env_requirements,
            disabled,
        } => {
            let body = serde_json::to_string(&McpServerCreate {
                id,
                name,
                command,
                args,
                env_requirements,
                enabled: !disabled,
            })
            .unwrap();
            let resp = client
                .post(format!("{base}/v1/mcp-servers"))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .context("create mcp-server request failed")?;
            err_if_fail(resp.status(), "create mcp-server")?;
            let s: McpServer = resp.json().await.context("bad mcp json")?;
            println!("mcp server {} registered: {}", s.id, s.name);
            Ok(())
        }
        McpAction::Delete { id } => {
            let resp = client
                .delete(format!("{base}/v1/mcp-servers/{}", id))
                .send()
                .await
                .context("delete mcp-server request failed")?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("mcp server {} not found", id);
            }
            err_if_fail(resp.status(), "delete mcp-server")?;
            println!("mcp server {} deleted", id);
            Ok(())
        }
        McpAction::Scan { id } => {
            let resp = client
                .get(format!("{base}/v1/mcp-servers"))
                .send()
                .await
                .context("list mcp-servers request failed")?;
            err_if_fail(resp.status(), "list mcp-servers")?;
            let servers: Vec<McpServer> = resp.json().await.context("bad mcp json")?;
            let s = servers
                .iter()
                .find(|s| s.id == id)
                .ok_or_else(|| anyhow::anyhow!("mcp server {id} not found"))?;
            use agentgrid_skills::scanner::{render_findings, scan_content};
            let mut text = s.command.clone();
            for a in &s.args {
                text.push(' ');
                text.push_str(a);
            }
            let findings = scan_content(&text);
            if findings.is_empty() {
                println!("mcp server {id} ({}): clean", s.name);
                return Ok(());
            }
            print!("{}", render_findings(&findings));
            let critical = findings
                .iter()
                .filter(|x| x.severity == agentgrid_skills::scanner::Severity::Critical)
                .count();
            if critical > 0 {
                anyhow::bail!("mcp server {id} has {critical} critical finding(s)");
            }
            Ok(())
        }
    }
}

async fn set_skill_trust(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    source: &str,
    decision: &str,
) -> Result<()> {
    let resp = client
        .post(format!(
            "{base}/v1/skills/{name}/{decision}?source={source}"
        ))
        .send()
        .await
        .context("skill trust request failed")?;
    err_if_fail(resp.status(), "skill trust")?;
    println!("skill {name} ({source}) -> {decision}");
    Ok(())
}

/// Stage 13: agent profile management. Revisions are immutable; activating an
/// older revision rolls back without losing history.
pub(crate) async fn cmd_profiles(
    client: &reqwest::Client,
    base: &str,
    a: ProfilesArgs,
) -> Result<()> {
    match a.action {
        ProfilesAction::List => {
            let resp = client
                .get(format!("{base}/v1/profiles"))
                .send()
                .await
                .context("profiles list request failed")?;
            err_if_fail(resp.status(), "profiles list")?;
            let ids: Vec<String> = resp.json().await.context("bad profiles json")?;
            if ids.is_empty() {
                println!("no active profiles");
            }
            for id in &ids {
                println!("{id}");
            }
            Ok(())
        }
        ProfilesAction::Show { id } => {
            let resp = client
                .get(format!("{base}/v1/profiles/{id}"))
                .send()
                .await
                .context("profile show request failed")?;
            err_if_fail(resp.status(), "profile show")?;
            let revs: Vec<AgentProfile> = resp.json().await.context("bad profile json")?;
            if revs.is_empty() {
                println!("profile {id}: no revisions");
            }
            for p in &revs {
                println!(
                    "{:<8}{:<2} {:<8} mem={:?} cpu={:?} tasks={:?} {}",
                    format!("r{}", p.revision),
                    if p.active { "*" } else { " " },
                    p.autonomy,
                    p.memory_max.map(|v| v.to_string()),
                    p.cpu_quota.map(|v| v.to_string()),
                    p.tasks_max.map(|v| v.to_string()),
                    if p.system_prompt.is_empty() {
                        ""
                    } else {
                        "<prompt>"
                    },
                );
            }
            Ok(())
        }
        ProfilesAction::Create(a) => {
            let id = a.id.clone();
            let mut secret_requirements = Vec::new();
            for e in &a.secret_required {
                secret_requirements.push(serde_json::json!({ "env": e, "required": true }));
            }
            for e in &a.secret_optional {
                secret_requirements.push(serde_json::json!({ "env": e, "required": false }));
            }
            let body = serde_json::json!({
                "system_prompt": a.system_prompt,
                "autonomy": a.autonomy,
                "memory_max": a.memory_max,
                "cpu_quota": a.cpu_quota,
                "tasks_max": a.tasks_max,
                "secret_requirements": secret_requirements,
                "adapter_version": a.adapter_version,
            });
            let resp = client
                .post(format!("{base}/v1/profiles/{}", a.id))
                .json(&body)
                .send()
                .await
                .context("profile create request failed")?;
            err_if_fail(resp.status(), "profile create")?;
            let v: serde_json::Value = resp.json().await.context("bad profile json")?;
            println!(
                "created {id}/r{} (not active; `ag profiles activate {id} <rev>`)",
                v["revision"]
            );
            Ok(())
        }
        ProfilesAction::Activate { id, revision } => {
            let resp = client
                .post(format!("{base}/v1/profiles/{id}/activate"))
                .json(&serde_json::json!({ "revision": revision }))
                .send()
                .await
                .context("profile activate request failed")?;
            err_if_fail(resp.status(), "profile activate")?;
            println!("activated {id}/r{revision}");
            Ok(())
        }
    }
}

// ── Feature "opencode profiles": CLI for opencode-config management ──────
#[derive(Args)]
pub(crate) struct OpencodeArgs {
    #[command(subcommand)]
    action: OpencodeAction,
}

#[derive(Subcommand)]
pub(crate) enum OpencodeAction {
    /// List all stored profiles.
    List,
    /// Show one profile by name (with the config payload).
    Show { name: String },
    /// Create or replace a profile from a JSON file (`-` for stdin).
    Set {
        name: String,
        #[arg(long, value_name = "FILE")]
        config: String,
        /// Optional absolute expiry (RFC3339 UTC, e.g. 2026-01-01T00:00:00Z);
        /// the profile is auto-deleted after this. Absent = never expires.
        #[arg(long, value_name = "RFC3339")]
        expires_at: Option<String>,
        /// Pin agentgrid skill names to this profile; on apply the node
        /// reconciles them against the trust ledger (repeatable).
        #[arg(long = "pin", value_name = "NAME")]
        pinned_skills: Vec<String>,
    },
    /// Delete a profile. Nodes keep their last-applied on-disk config,
    /// unless `--fallback <name>` re-points them onto another profile first.
    Delete {
        name: String,
        /// Reassign every node currently on this profile to another profile.
        #[arg(long, value_name = "NAME")]
        fallback: Option<String>,
    },
    /// Swap the profile back one revision (PUT-with-history keeps one step).
    Rollback {
        name: String,
        /// Walk back N revisions instead of one (≤32; deeper walks need
        /// `--steps=N` on this flag — the API caps at 32 per call).
        #[arg(long, default_value_t = 1)]
        steps: u32,
    },
    /// Assign (or `--clear`) the profile a node applies.
    Assign {
        node_id: String,
        /// Profile name; omit with `--clear` to detach.
        #[arg(long, conflicts_with = "clear")]
        profile: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Show recent apply events for a node.
    Audit { node_id: String },
    /// A/B split: move the nodes on either <name> or <other> so N% land
    /// on <name> and the rest on <other>.
    Ab {
        name: String,
        #[arg(long, value_name = "NAME")]
        other: String,
        /// Share of nodes for <name> (0-100).
        #[arg(long)]
        percent: u8,
    },
}

pub(crate) async fn cmd_opencode(
    client: &reqwest::Client,
    _base: &str,
    a: OpencodeArgs,
) -> Result<()> {
    use agentgrid_common::{ListResponse, OpencodeProfile};
    let base = _base; // keep the conventional arg name out of shadowing trouble
    match a.action {
        OpencodeAction::List => {
            let resp = client
                .get(format!("{base}/v1/opencode-profiles"))
                .send()
                .await
                .context("list opencode profiles")?;
            err_if_fail(resp.status(), "list")?;
            let items: ListResponse<OpencodeProfile> = resp.json().await?;
            for p in &items.items {
                println!(
                    "{:<16} {} {} {} bytes {}",
                    p.name,
                    &p.hash[..12],
                    p.updated_at,
                    p.config.to_string().len(),
                    p.apply_count
                        .map(|n| format!("{n} applies"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        OpencodeAction::Show { name } => {
            let resp = client
                .get(format!("{base}/v1/opencode-profiles/{name}"))
                .send()
                .await?;
            err_if_fail(resp.status(), "opencode profile show")?;
            let p: OpencodeProfile = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&p)?);
            Ok(())
        }
        OpencodeAction::Set {
            name,
            config,
            expires_at,
            pinned_skills,
        } => {
            let content = if config == "-" {
                use tokio::io::AsyncReadExt;
                let mut b = String::new();
                tokio::io::stdin().read_to_string(&mut b).await?;
                b
            } else {
                std::fs::read_to_string(&config).context("read config file")?
            };
            let cfg: serde_json::Value = serde_json::from_str(&content).context("invalid JSON")?;
            let resp = client
                .put(format!("{base}/v1/opencode-profiles/{name}"))
                .json(&serde_json::json!({
                    "config": cfg,
                    "expires_at": expires_at,
                    "pinned_skills": pinned_skills,
                }))
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!(
                    "upsert failed: {} {}",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                );
            }
            let p: OpencodeProfile = resp.json().await?;
            println!(
                "profile {} hash={} updated_at={}",
                p.name,
                &p.hash[..12],
                p.updated_at
            );
            Ok(())
        }
        OpencodeAction::Delete { name, fallback } => {
            let mut url = format!("{base}/v1/opencode-profiles/{name}");
            if let Some(fb) = &fallback {
                url.push_str(&format!("?fallback={fb}"));
            }
            let resp = client.delete(url).send().await?;
            match resp.status() {
                reqwest::StatusCode::NO_CONTENT => {
                    match fallback {
                        Some(fb) => println!("deleted {name}; nodes moved to {fb}"),
                        None => println!("deleted {name}"),
                    }
                    Ok(())
                }
                other => anyhow::bail!("delete failed: {other}"),
            }
        }
        OpencodeAction::Rollback { name, steps } => {
            let resp = client
                .post(format!(
                    "{base}/v1/opencode-profiles/{name}/rollback?steps={steps}"
                ))
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await?;
            err_if_fail(resp.status(), "rollback")?;
            let p: OpencodeProfile = resp.json().await?;
            println!(
                "rolled back {} {steps} step{} -> hash {}",
                p.name,
                if steps == 1 { "" } else { "s" },
                &p.hash[..12]
            );
            Ok(())
        }
        OpencodeAction::Assign {
            node_id,
            profile,
            clear,
        } => {
            let profile_id = if clear {
                None
            } else {
                let Some(name) = profile else {
                    anyhow::bail!("pass --profile <name> or --clear");
                };
                // Resolve the profile id.
                let resp = client
                    .get(format!("{base}/v1/opencode-profiles/{name}"))
                    .send()
                    .await?;
                err_if_fail(resp.status(), "profile {name}")?;
                let p: OpencodeProfile = resp.json().await?;
                Some(p.id)
            };
            let body = serde_json::json!({ "profile_id": profile_id });
            let resp = client
                .post(format!("{base}/v1/nodes/{node_id}/opencode-profile"))
                .json(&body)
                .send()
                .await?;
            err_if_fail(resp.status(), "assign")?;
            if clear {
                println!("cleared profile for node {node_id}");
            } else {
                println!(
                    "assigned profile {} to node {}",
                    profile_id.as_deref().unwrap_or("?"),
                    node_id
                );
            }
            Ok(())
        }
        OpencodeAction::Audit { node_id } => {
            let resp = client
                .get(format!("{base}/v1/nodes/{node_id}/opencode-audit"))
                .send()
                .await?;
            err_if_fail(resp.status(), "audit")?;
            let audits: ListResponse<agentgrid_common::OpencodeConfigAuditEntry> =
                resp.json().await?;
            for a in &audits.items {
                let pin = a
                    .pinned_untrusted
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .map(|v| v.join(","))
                    .unwrap_or_default();
                println!(
                    "{} profile={} hash={} trigger={}{}{}",
                    a.at,
                    a.profile_id.as_deref().unwrap_or("-"),
                    &a.hash[..12.min(a.hash.len())],
                    a.trigger,
                    if pin.is_empty() {
                        String::new()
                    } else {
                        " untrusted_pins=".to_string()
                    },
                    pin,
                );
            }
            Ok(())
        }
        OpencodeAction::Ab {
            name,
            other,
            percent,
        } => {
            let resp = client
                .post(format!("{base}/v1/opencode-profiles/{name}/assign-percent"))
                .json(&serde_json::json!({ "other": other, "percent": percent }))
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!(
                    "A/B failed: {} {}",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                );
            }
            let v: serde_json::Value = resp.json().await?;
            let moved = v.get("moved").and_then(|m| m.as_u64()).unwrap_or(0);
            println!("A/B {name} vs {other}: {moved} nodes split ({percent}% on {name})");
            Ok(())
        }
    }
}

// ── CP-managed egress proxy pool ───────────────────────────────────────

/// `ag proxy ls/add/rm` — manage the proxy list the CP pushes to nodes.
#[derive(Args)]
pub(crate) struct ProxyArgs {
    #[command(subcommand)]
    action: ProxyAction,
}

#[derive(Subcommand)]
pub(crate) enum ProxyAction {
    /// List registered proxies (global pool first).
    Ls,
    /// Add a proxy URL (`http://user:pass@host:port`, `socks5://…`).
    Add {
        url: String,
        /// Restrict this proxy to one node id (global pool otherwise).
        #[arg(long)]
        node: Option<String>,
    },
    /// Remove a proxy by id (see `ag proxy ls`).
    Rm { id: i64 },
}

pub(crate) async fn cmd_proxy(client: &reqwest::Client, base: &str, a: ProxyArgs) -> Result<()> {
    match a.action {
        ProxyAction::Ls => {
            let resp = client.get(format!("{base}/v1/proxies")).send().await?;
            err_if_fail(resp.status(), "list proxies")?;
            let v: serde_json::Value = resp.json().await?;
            for p in list_items(&v) {
                println!(
                    "#{:>3} {:<40} node={}",
                    p["id"].as_i64().unwrap_or(0),
                    p["url"].as_str().unwrap_or(""),
                    p["node_id"].as_str().unwrap_or("*"),
                );
            }
            Ok(())
        }
        ProxyAction::Add { url, node } => {
            let resp = client
                .post(format!("{base}/v1/proxies"))
                .json(&serde_json::json!({ "url": url, "node_id": node }))
                .send()
                .await?;
            err_if_fail(resp.status(), "add proxy")?;
            let v: serde_json::Value = resp.json().await?;
            println!("proxy #{} registered", v["id"]);
            Ok(())
        }
        ProxyAction::Rm { id } => {
            let resp = client
                .delete(format!("{base}/v1/proxies/{id}"))
                .send()
                .await?;
            err_if_fail(resp.status(), "remove proxy")?;
            println!("proxy #{id} removed");
            Ok(())
        }
    }
}
