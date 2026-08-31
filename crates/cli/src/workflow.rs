//! `ag workflow …` and `ag run --workflow <path>` — extracted from main.rs
//! in the CLI monolith split. Handlers only; shared helpers live in main.rs.

use agentgrid_common::{
    CreateWorkflowRequest, CreateWorkflowRunRequest, WorkflowStep, WorkflowTemplate,
};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::{api_error, err_if_fail, list_items};

#[derive(Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    command: WorkflowSub,
}

#[derive(Subcommand)]
enum WorkflowSub {
    /// Define a workflow template from a steps JSON file.
    Create(WorkflowCreateArgs),
    /// List workflow templates.
    List,
    /// Show a workflow template (its DAG).
    Show(WorkflowShowArgs),
    /// Start a run of a template.
    Run(WorkflowRunArgs),
    /// Cancel a whole workflow run (and its non-terminal steps/tasks).
    Cancel(WorkflowCancelArgs),
    /// Manage scheduled/recurring triggers for a workflow template (Stage 13).
    Schedules(WorkflowSchedulesArgs),
    /// Plan 1.9 (#17): validate a `.agentgrid/workflows/*.yaml` file against
    /// the strict schema + DAG (no server round-trip). Exit code 1 on error.
    Validate(WorkflowValidateArgs),
}

#[derive(Args)]
struct WorkflowCreateArgs {
    #[arg(long)]
    name: String,
    /// Path to a JSON file: an array of WorkflowStep objects.
    #[arg(long)]
    steps: String,
    /// Optional default context JSON.
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args)]
struct WorkflowShowArgs {
    template_id: String,
}

#[derive(Args)]
struct WorkflowRunArgs {
    template_id: String,
    /// Optional run context JSON (overrides the template default).
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args)]
struct WorkflowSchedulesArgs {
    id: String,
    #[command(subcommand)]
    action: SchedulesAction,
}

#[derive(Args)]
struct WorkflowValidateArgs {
    /// Path to the `.agentgrid/workflows/*.yaml` file to validate.
    path: String,
    /// CSV of adapters allowed for this repo's workflows (e.g. `mock,claude`).
    /// When set, a step naming an adapter not in the list fails validation.
    #[arg(long)]
    adapters: Option<String>,
}

#[derive(Subcommand)]
enum SchedulesAction {
    /// List schedules for a template.
    List,
    /// Create a scheduled trigger.
    Create {
        /// Interval between runs in seconds (>=1).
        #[arg(long)]
        interval_seconds: i64,
        /// Autonomy level l0..l4 (default l2).
        #[arg(long, default_value = "l2")]
        autonomy: String,
        /// Start paused (default: enabled).
        #[arg(long)]
        paused: bool,
    },
    /// Delete a schedule.
    Delete { sid: String },
}

#[derive(Args)]
struct WorkflowCancelArgs {
    /// Workflow run id to cancel.
    id: String,
}

pub(crate) async fn cmd_workflow(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowArgs,
    json: bool,
) -> Result<()> {
    match a.command {
        WorkflowSub::Create(c) => cmd_workflow_create(client, base, c).await,
        WorkflowSub::List => cmd_workflow_list(client, base, json).await,
        WorkflowSub::Show(s) => cmd_workflow_show(client, base, s, json).await,
        WorkflowSub::Run(r) => cmd_workflow_run(client, base, r).await,
        WorkflowSub::Validate(v) => cmd_workflow_validate(v).await,
        WorkflowSub::Cancel(c) => cmd_workflow_cancel(client, base, c).await,
        WorkflowSub::Schedules(s) => cmd_workflow_schedules(client, base, s).await,
    }
}

async fn cmd_workflow_create(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowCreateArgs,
) -> Result<()> {
    let body = std::fs::read_to_string(&a.steps).with_context(|| format!("read {}", a.steps))?;
    let steps: Vec<WorkflowStep> = serde_json::from_str(&body)
        .with_context(|| format!("parse steps JSON from {}", a.steps))?;
    let req = CreateWorkflowRequest {
        name: a.name,
        steps,
        context: a.context,
        budget: None,
    };
    let resp = client
        .post(format!("{base}/v1/workflows"))
        .json(&req)
        .send()
        .await
        .context("create workflow request failed")?;
    err_if_fail(resp.status(), "create workflow")?;
    let tpl: WorkflowTemplate = resp.json().await.context("parse workflow response")?;
    println!("workflow {} created ({} steps)", tpl.id, tpl.steps.len());
    println!("{}", tpl.id);
    Ok(())
}

/// Plan 1.9 (#17): local schema + DAG validation of a workflow YAML file — no
/// server round-trip, so CI can gate on it. Exit code 1 on error.
async fn cmd_workflow_validate(v: WorkflowValidateArgs) -> Result<()> {
    let allowed = v
        .adapters
        .as_deref()
        .map(|csv| {
            csv.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());
    match WorkflowTemplate::read_workflow_yaml(&v.path, allowed.as_deref()) {
        Ok(tmpl) => {
            println!("{}: valid ({} steps)", v.path, tmpl.steps.len());
            Ok(())
        }
        Err(e) => {
            eprintln!("{}: invalid", v.path);
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_list(client: &reqwest::Client, base: &str, json: bool) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/workflows"))
        .send()
        .await
        .context("list workflows request failed")?;
    let v: serde_json::Value = resp.json().await.context("parse workflows response")?;
    let tpls: Vec<WorkflowTemplate> =
        serde_json::from_value(serde_json::Value::Array(list_items(&v)))
            .context("parse workflows response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tpls)?);
        return Ok(());
    }
    if tpls.is_empty() {
        println!("(no workflows)");
        return Ok(());
    }
    for t in &tpls {
        println!("{}\t{}\t{} steps", t.id, t.name, t.steps.len());
    }
    Ok(())
}

async fn cmd_workflow_show(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowShowArgs,
    json: bool,
) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/workflows/{}", a.template_id))
        .send()
        .await
        .context("show workflow request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("workflow {} not found", a.template_id);
    }
    let tpl: WorkflowTemplate = resp.json().await.context("parse workflow response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tpl)?);
        return Ok(());
    }
    println!("workflow {}", tpl.id);
    println!("name: {}", tpl.name);
    println!("steps:");
    for s in &tpl.steps {
        println!(
            "  - {} [{}] deps={:?}",
            s.id,
            format!("{:?}", s.role).to_lowercase(),
            s.depends_on
        );
    }
    Ok(())
}

async fn cmd_workflow_cancel(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowCancelArgs,
) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/workflow-runs/{}/cancel", a.id))
        .send()
        .await
        .context("cancel workflow run request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("workflow run {} not found", a.id);
    }
    if !resp.status().is_success() {
        return Err(api_error(resp.status(), "cancel workflow run"));
    }
    println!("workflow run {} cancelled", a.id);
    Ok(())
}

async fn cmd_workflow_schedules(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowSchedulesArgs,
) -> Result<()> {
    use agentgrid_common::WorkflowSchedule;
    match a.action {
        SchedulesAction::List => {
            let resp = client
                .get(format!("{base}/v1/workflows/{}/schedules", a.id))
                .send()
                .await
                .context("list schedules request failed")?;
            err_if_fail(resp.status(), "list schedules")?;
            let v: serde_json::Value = resp.json().await.context("bad schedule json")?;
            let schedules: Vec<WorkflowSchedule> =
                serde_json::from_value(serde_json::Value::Array(list_items(&v)))
                    .context("bad schedule json")?;
            if schedules.is_empty() {
                println!("no schedules for {}", a.id);
            }
            for s in &schedules {
                println!(
                    "{:<12} interval={}s autonomy={} {} last={}",
                    s.id,
                    s.interval_seconds,
                    s.autonomy,
                    if s.enabled { "[on]" } else { "[off]" },
                    if s.last_run_at.is_empty() {
                        "-"
                    } else {
                        &s.last_run_at
                    }
                );
            }
            Ok(())
        }
        SchedulesAction::Create {
            interval_seconds,
            autonomy,
            paused,
        } => {
            let body = serde_json::json!({
                "interval_seconds": interval_seconds,
                "autonomy": autonomy,
                "enabled": !paused,
            });
            let resp = client
                .post(format!("{base}/v1/workflows/{}/schedules", a.id))
                .json(&body)
                .send()
                .await
                .context("create schedule request failed")?;
            err_if_fail(resp.status(), "create schedule")?;
            let s: WorkflowSchedule = resp.json().await.context("bad schedule json")?;
            println!(
                "schedule {} created: interval={}s autonomy={} {}",
                s.id,
                s.interval_seconds,
                s.autonomy,
                if s.enabled { "[on]" } else { "[off]" }
            );
            Ok(())
        }
        SchedulesAction::Delete { sid } => {
            let resp = client
                .delete(format!("{base}/v1/workflows/{}/schedules/{}", a.id, sid))
                .send()
                .await
                .context("delete schedule request failed")?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("schedule {} not found", sid);
            }
            err_if_fail(resp.status(), "delete schedule")?;
            println!("schedule {} deleted", sid);
            Ok(())
        }
    }
}

async fn cmd_workflow_run(client: &reqwest::Client, base: &str, a: WorkflowRunArgs) -> Result<()> {
    let req = CreateWorkflowRunRequest {
        context: a.context,
        repository: None,
        base_commit: None,
    };
    let resp = client
        .post(format!("{base}/v1/workflows/{}/runs", a.template_id))
        .json(&req)
        .send()
        .await
        .context("create workflow run request failed")?;
    err_if_fail(resp.status(), "create workflow run")?;
    let run: agentgrid_common::WorkflowRun =
        resp.json().await.context("parse workflow run response")?;
    println!("workflow run {} started (status: {:?})", run.id, run.status);
    println!("{}", run.id);
    Ok(())
}

/// Plan 1.9 (#17): `ag run --workflow <path|dir>` — validate the YAML locally,
/// create the template via the API, then start a run.
pub(crate) async fn cmd_run_workflow(
    client: &reqwest::Client,
    base: &str,
    repository: String,
    wf: &str,
) -> Result<()> {
    let path = resolve_workflow_path(wf)?;
    let tmpl = WorkflowTemplate::read_workflow_yaml(&path, None)
        .map_err(|e| anyhow::anyhow!("validate {path}: {e}"))?;
    let req = CreateWorkflowRequest {
        name: tmpl.name.clone(),
        steps: tmpl.steps.clone(),
        context: None,
        budget: tmpl.budget.clone(),
    };
    let resp = client
        .post(format!("{base}/v1/workflows"))
        .json(&req)
        .send()
        .await
        .context("create workflow request failed")?;
    err_if_fail(resp.status(), "create workflow")?;
    let created: WorkflowTemplate = resp.json().await.context("parse workflow response")?;
    let run_req = CreateWorkflowRunRequest {
        context: None,
        repository: Some(repository),
        base_commit: None,
    };
    let resp = client
        .post(format!("{base}/v1/workflows/{}/runs", created.id))
        .json(&run_req)
        .send()
        .await
        .context("create workflow run request failed")?;
    err_if_fail(resp.status(), "create workflow run")?;
    let run: agentgrid_common::WorkflowRun =
        resp.json().await.context("parse workflow run response")?;
    println!(
        "workflow {} run {} started ({} steps, status: {:?})",
        created.id,
        run.id,
        created.steps.len(),
        run.status
    );
    println!("{}", run.id);
    Ok(())
}

/// Resolve `--workflow <path|dir>`: a directory means the first `*.yaml`
/// inside it; anything else is used as-is. Errors when a directory has none.
fn resolve_workflow_path(wf: &str) -> Result<String> {
    let p = std::path::Path::new(wf);
    if p.is_dir() {
        let mut found: Vec<String> = std::fs::read_dir(p)
            .with_context(|| format!("read workflow dir {wf}"))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x == "yaml" || x == "yml")
            })
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no *.yaml workflow files in {wf}"))
    } else if p.exists() {
        Ok(wf.to_string())
    } else {
        Err(anyhow::anyhow!("workflow path {wf} does not exist"))
    }
}

#[cfg(test)]
mod workflow_file_tests {
    use super::*;

    #[test]
    fn resolve_workflow_path_dir_picks_first_yaml() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ag-wfdir-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.yaml"), "").unwrap();
        std::fs::write(dir.join("a.yaml"), "").unwrap();
        let got = resolve_workflow_path(&dir.to_string_lossy()).unwrap();
        assert!(got.ends_with("a.yaml"), "got: {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_workflow_path_dir_without_yaml_errors() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ag-wfdir2-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let err = resolve_workflow_path(&dir.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("no *.yaml"), "err: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_workflow_path_missing_file_errors() {
        let err = resolve_workflow_path("/nonexistent/wf.yaml").unwrap_err();
        assert!(err.to_string().contains("does not exist"), "err: {err}");
    }
}
