//! Eval harness (backlog item 7): run a fixture of tasks and report pass rate,
//! tokens and cost — optionally against a baseline, so a regression is a number
//! rather than a feeling.
//!
//! `crates/node-daemon/src/evals.rs` is a *self-healing validation* loop
//! (pinned commands, tamper-checked), not a measurement: nothing produced a
//! score, so "did this change make the agent worse, or just different?" had no
//! answer. This module adds the measurement half and deliberately stays a thin
//! driver over the ordinary task API — an eval case *is* a task, so whatever
//! the control plane and the nodes do to a real task (retries, budgets,
//! validation feedback) applies to the case unchanged.
//!
//! A fixture is JSON:
//!
//! ```json
//! {
//!   "adapter": "opencode",
//!   "cases": [
//!     { "name": "rename-helper", "repository": "demo", "prompt": "…",
//!       "base_commit": "abc123", "validation_command": "cargo test",
//!       "expect": "pass" }
//!   ]
//! }
//! ```
//!
//! `adapter` is required at the fixture level and may be overridden per case:
//! it is the variable an eval is usually trying to measure, so defaulting it
//! (to `mock`, say) would report a pass rate for something that never ran.
//!
//! Cost and tokens are summed from the attempt's `metric` events, which carry
//! per-event increments (`tokens` / `cost_cents`) — the same numbers the budget
//! enforcement uses, so an eval cannot disagree with the billing.

use std::collections::HashMap;
use std::time::Duration;

use agentgrid_common::{CreateTaskRequest, EventType, TaskEvent, TaskStatus, TaskView};
use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Args)]
pub(crate) struct EvalArgs {
    /// Fixture file (JSON) with the cases to run.
    fixture: String,
    /// Compare against a previous run's `--json` output and report regressions.
    #[arg(long)]
    baseline: Option<String>,
    /// Give up on a single case after this many seconds.
    #[arg(long, default_value_t = 1800)]
    case_timeout: u64,
}

/// What a case is expected to do. `pass` is the default; `fail` pins a known
/// limitation, so a fixture can record "this must NOT succeed" instead of
/// silently dropping the case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Expect {
    #[default]
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvalCase {
    pub name: String,
    pub prompt: String,
    /// Repository the task runs against (must be known to the control plane).
    pub repository: String,
    /// Override the fixture's adapter for this case only (e.g. one case that
    /// only `claude` can attempt).
    #[serde(default)]
    pub adapter: Option<String>,
    /// Pin the tree the case starts from, so a moving default branch cannot
    /// turn a code regression into a "different code" result.
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub validation_command: Option<String>,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Fixture {
    /// Adapter every case runs on unless the case names its own.
    pub adapter: String,
    pub cases: Vec<EvalCase>,
}

impl Fixture {
    /// Adapter for `case`: the case's override, else the fixture default.
    fn adapter_for<'a>(&'a self, case: &'a EvalCase) -> &'a str {
        case.adapter.as_deref().unwrap_or(&self.adapter)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CaseResult {
    pub name: String,
    /// Terminal status reported by the control plane.
    pub status: String,
    /// True when the status matched `expect`.
    pub ok: bool,
    pub tokens: u64,
    pub cost_cents: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Score {
    pub cases: Vec<CaseResult>,
}

impl Score {
    pub(crate) fn total(&self) -> usize {
        self.cases.len()
    }

    pub(crate) fn passed(&self) -> usize {
        self.cases.iter().filter(|c| c.ok).count()
    }

    /// Fraction of cases that met their expectation, 0.0 for an empty run.
    pub(crate) fn pass_rate(&self) -> f64 {
        match self.total() {
            0 => 0.0,
            n => self.passed() as f64 / n as f64,
        }
    }

    pub(crate) fn tokens(&self) -> u64 {
        self.cases.iter().map(|c| c.tokens).sum()
    }

    pub(crate) fn cost_cents(&self) -> u64 {
        self.cases.iter().map(|c| c.cost_cents).sum()
    }
}

/// Regressions of `now` against `base`, as human-readable lines.
///
/// Two thresholds, both deliberately blunt: a case that passed and no longer
/// does is always a regression; cost is only flagged when it grows by more than
/// [`COST_REGRESSION_PCT`], because a few cents of variance between runs is
/// noise, not signal. A case present in the baseline but absent now is
/// reported too — dropping a hard case is the cheapest way to "improve" a pass
/// rate.
pub(crate) fn compare(now: &Score, base: &Score) -> Vec<String> {
    const COST_REGRESSION_PCT: u64 = 25;
    let mut out = Vec::new();
    let prev: HashMap<&str, &CaseResult> =
        base.cases.iter().map(|c| (c.name.as_str(), c)).collect();
    for c in &now.cases {
        match prev.get(c.name.as_str()) {
            Some(b) if b.ok && !c.ok => {
                out.push(format!("{}: passed before, now {}", c.name, c.status))
            }
            Some(b) if !b.ok && c.ok => {
                out.push(format!("{}: was failing, now passes (improvement)", c.name))
            }
            _ => {}
        }
    }
    let seen: HashMap<&str, ()> = now.cases.iter().map(|c| (c.name.as_str(), ())).collect();
    for b in &base.cases {
        if !seen.contains_key(b.name.as_str()) {
            out.push(format!("{}: in the baseline, missing now", b.name));
        }
    }
    let (bt, nt) = (base.cost_cents(), now.cost_cents());
    if bt > 0 && nt > bt + bt * COST_REGRESSION_PCT / 100 {
        out.push(format!(
            "cost: {}c -> {}c ({}% over baseline)",
            bt,
            nt,
            (nt - bt) * 100 / bt
        ));
    }
    out
}

/// The `--json` document: the aggregate a human reads *and* the exact shape a
/// later run needs to diff against, so one file serves as both report and
/// baseline. `Score` reads only `cases` and ignores the rest, which is what
/// makes that round trip work — [`a_json_report_is_also_a_valid_baseline`]
/// pins it.
fn report_json(score: &Score, diffs: &[String]) -> serde_json::Value {
    json!({
        "total": score.total(),
        "passed": score.passed(),
        "pass_rate": score.pass_rate(),
        "tokens": score.tokens(),
        "cost_cents": score.cost_cents(),
        "diffs": diffs,
        "cases": score.cases,
    })
}

pub(crate) async fn cmd_eval(
    client: &reqwest::Client,
    base: &str,
    a: EvalArgs,
    json: bool,
) -> Result<()> {
    let raw = std::fs::read_to_string(&a.fixture)
        .with_context(|| format!("read fixture {}", a.fixture))?;
    let fixture: Fixture = serde_json::from_str(&raw).context("parse fixture JSON")?;
    if fixture.cases.is_empty() {
        anyhow::bail!("fixture has no cases");
    }
    if fixture.adapter.trim().is_empty() {
        // Not defaulted: an empty adapter is a 400 at the API, and a made-up
        // one would score a run that never happened.
        anyhow::bail!("fixture needs an `adapter` (e.g. \"opencode\")");
    }
    let mut score = Score::default();
    for case in &fixture.cases {
        let adapter = fixture.adapter_for(case);
        let res = run_case(client, base, case, adapter, a.case_timeout).await?;
        let line = format!(
            "{} {:<28} {:<10} tokens={} cost={}c",
            if res.ok { "PASS" } else { "FAIL" },
            res.name,
            res.status,
            res.tokens,
            res.cost_cents
        );
        // With `--json`, stdout must stay a single parseable document, so
        // progress goes to stderr — a case can run for half an hour and CI
        // logs still need to show which one is hanging.
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
        score.cases.push(res);
    }

    let mut diffs: Vec<String> = Vec::new();
    if let Some(path) = &a.baseline {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read baseline {path}"))?;
        let baseline: Score = serde_json::from_str(&raw)
            .with_context(|| format!("parse baseline {path} (a `--json` run)"))?;
        diffs = compare(&score, &baseline);
        if !json {
            if diffs.is_empty() {
                println!("\nno regressions against {path}");
            } else {
                println!("\nagainst {path}:");
                for d in &diffs {
                    println!("  {d}");
                }
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&score, &diffs))?
        );
    } else {
        println!(
            "\npass {}/{} ({:.0}%) · tokens {} · cost {}c",
            score.passed(),
            score.total(),
            score.pass_rate() * 100.0,
            score.tokens(),
            score.cost_cents()
        );
    }

    // A regression is an operator decision, not a silent note. Both exits are
    // non-zero in JSON mode too: CI reads the status, not the document.
    if !diffs.is_empty() {
        anyhow::bail!("{} difference(s) against the baseline", diffs.len());
    }
    if score.passed() < score.total() {
        anyhow::bail!(
            "{} of {} case(s) did not meet their expectation",
            score.total() - score.passed(),
            score.total()
        );
    }
    Ok(())
}

async fn run_case(
    client: &reqwest::Client,
    base: &str,
    case: &EvalCase,
    adapter: &str,
    timeout_secs: u64,
) -> Result<CaseResult> {
    let req = CreateTaskRequest {
        prompt: case.prompt.clone(),
        repository: case.repository.clone(),
        adapter: adapter.to_string(),
        base_commit: case.base_commit.clone(),
        validation_command: case.validation_command.clone(),
        ..Default::default()
    };
    let resp = client
        .post(format!("{base}/v1/tasks"))
        .json(&req)
        .send()
        .await
        .context("submit eval case")?;
    if !resp.status().is_success() {
        anyhow::bail!("submit {} failed ({})", case.name, resp.status());
    }
    let created: TaskView = resp.json().await.context("parse created task")?;
    let view = wait_terminal(client, base, &created.id, timeout_secs).await?;
    let (tokens, cost_cents) = usage(client, base, &created.id).await?;
    let status = format!("{:?}", view.status).to_lowercase();
    let ok = match case.expect {
        Expect::Pass => view.status == TaskStatus::Succeeded,
        Expect::Fail => view.status != TaskStatus::Succeeded,
    };
    Ok(CaseResult {
        name: case.name.clone(),
        status,
        ok,
        tokens,
        cost_cents,
    })
}

async fn wait_terminal(
    client: &reqwest::Client,
    base: &str,
    task_id: &str,
    timeout_secs: u64,
) -> Result<TaskView> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let resp = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .send()
            .await
            .context("poll eval case")?;
        if !resp.status().is_success() {
            anyhow::bail!("poll {task_id} failed ({})", resp.status());
        }
        let view: TaskView = resp.json().await.context("parse task view")?;
        if matches!(
            view.status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Ok(view);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "case {task_id} still {:?} after {timeout_secs}s — the node never picked it up?",
                view.status
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Sum the per-event increments the budget enforcement also uses, so an eval
/// cannot disagree with what the task was billed.
///
/// Only `metric` events count, and only under `tokens` / `cost_cents`: those
/// are increments. A cumulative total under the same key (or a context gauge
/// like `used`) would inflate the score, and a score that disagrees with the
/// bill is worse than no score.
fn sum_usage<'a>(events: impl IntoIterator<Item = &'a TaskEvent>) -> (u64, u64) {
    let mut tokens = 0u64;
    let mut cost = 0u64;
    for e in events {
        if e.r#type != EventType::Metric {
            continue;
        }
        tokens += e
            .payload
            .get("tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        cost += e
            .payload
            .get("cost_cents")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    }
    (tokens, cost)
}

/// The server clamps `limit` to this, so paging must not assume a bigger page.
const EVENTS_PAGE: u64 = 1000;

/// Page the event log until it is exhausted, then sum.
///
/// A single request silently truncates at [`EVENTS_PAGE`] rows — a chatty
/// attempt emits far more than that, and the truncated tail is exactly where
/// the expensive steps are, so a one-shot read would under-report cost on the
/// runs that matter most. The cursor is `ingest_id` (global and monotonic), and
/// the loop also stops when the cursor fails to advance, which is what an old
/// server that never populated `ingest_id` looks like.
async fn usage(client: &reqwest::Client, base: &str, task_id: &str) -> Result<(u64, u64)> {
    let (mut tokens, mut cost) = (0u64, 0u64);
    let mut after: Option<u64> = None;
    loop {
        let mut query: Vec<(&str, u64)> = vec![("limit", EVENTS_PAGE)];
        if let Some(cursor) = after {
            query.push(("after_ingest", cursor));
        }
        let resp = client
            .get(format!("{base}/v1/tasks/{task_id}/events"))
            .query(&query)
            .send()
            .await
            .context("fetch eval events")?;
        if !resp.status().is_success() {
            // A missing usage number must not fail the case: the pass/fail
            // verdict is still valid, only the cost column is blank.
            return Ok((tokens, cost));
        }
        let events: Vec<TaskEvent> = resp.json().await.context("parse eval events")?;
        if events.is_empty() {
            return Ok((tokens, cost));
        }
        let (t, c) = sum_usage(&events);
        tokens += t;
        cost += c;
        let highest = events.iter().map(|e| e.ingest_id).max().unwrap_or(0);
        match after {
            Some(prev) if highest <= prev => return Ok((tokens, cost)),
            _ if highest == 0 => return Ok((tokens, cost)),
            _ => after = Some(highest),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, ok: bool, tokens: u64, cost_cents: u64) -> CaseResult {
        CaseResult {
            name: name.into(),
            status: if ok { "succeeded" } else { "failed" }.into(),
            ok,
            tokens,
            cost_cents,
        }
    }

    #[test]
    fn score_aggregates_pass_rate_tokens_and_cost() {
        let s = Score {
            cases: vec![
                case("a", true, 100, 5),
                case("b", false, 50, 2),
                case("c", true, 10, 1),
            ],
        };
        assert_eq!(s.total(), 3);
        assert_eq!(s.passed(), 2);
        assert!((s.pass_rate() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(s.tokens(), 160);
        assert_eq!(s.cost_cents(), 8);
    }

    #[test]
    fn an_empty_run_has_no_pass_rate() {
        // 0.0, not NaN and not 1.0: an empty fixture must not look perfect.
        assert_eq!(Score::default().pass_rate(), 0.0);
    }

    #[test]
    fn compare_flags_a_lost_pass_a_dropped_case_and_a_cost_jump() {
        let base = Score {
            cases: vec![case("a", true, 100, 10), case("b", true, 100, 10)],
        };
        let now = Score {
            cases: vec![case("a", false, 100, 10)],
        };
        let d = compare(&now, &base);
        assert!(d.iter().any(|x| x.contains("a: passed before")), "{d:?}");
        assert!(d.iter().any(|x| x.contains("b: in the baseline")), "{d:?}");
        assert!(
            !d.iter().any(|x| x.contains("cost:")),
            "cost did not rise: {d:?}"
        );
    }

    #[test]
    fn compare_tolerates_cost_noise_but_flags_a_real_jump() {
        let base = Score {
            cases: vec![case("a", true, 100, 100)],
        };
        let noisy = Score {
            cases: vec![case("a", true, 100, 110)],
        };
        assert!(compare(&noisy, &base).is_empty(), "10% is noise");
        let jumped = Score {
            cases: vec![case("a", true, 100, 150)],
        };
        let d = compare(&jumped, &base);
        assert!(d.iter().any(|x| x.contains("cost: 100c -> 150c")), "{d:?}");
    }

    #[test]
    fn fixture_parses_with_defaults() {
        let f: Fixture = serde_json::from_str(
            r#"{"adapter":"opencode","cases":[{"name":"x","prompt":"p","repository":"r"}]}"#,
        )
        .unwrap();
        assert_eq!(f.cases.len(), 1);
        assert_eq!(f.cases[0].expect, Expect::Pass);
        assert!(f.cases[0].base_commit.is_none());
        let f: Fixture = serde_json::from_str(
            r#"{"adapter":"opencode","cases":[{"name":"x","prompt":"p","repository":"r","expect":"fail"}]}"#,
        )
        .unwrap();
        assert_eq!(f.cases[0].expect, Expect::Fail);
    }

    #[test]
    fn a_case_adapter_overrides_the_fixture_adapter() {
        let f: Fixture = serde_json::from_str(
            r#"{"adapter":"opencode","cases":[
                 {"name":"a","prompt":"p","repository":"r"},
                 {"name":"b","prompt":"p","repository":"r","adapter":"claude"}
               ]}"#,
        )
        .unwrap();
        assert_eq!(f.adapter_for(&f.cases[0]), "opencode");
        assert_eq!(f.adapter_for(&f.cases[1]), "claude");
    }

    #[test]
    fn a_fixture_without_an_adapter_does_not_parse() {
        // Silently defaulting it would score a run that never happened.
        assert!(serde_json::from_str::<Fixture>(
            r#"{"cases":[{"name":"x","prompt":"p","repository":"r"}]}"#
        )
        .is_err());
    }

    #[test]
    fn a_json_report_is_also_a_valid_baseline() {
        // The bug this pins: `--json` used to print progress lines before the
        // document, so `ag eval f --json > base.json` produced a file that
        // `--baseline` then refused to parse.
        let score = Score {
            cases: vec![case("a", true, 100, 5)],
        };
        let text = serde_json::to_string_pretty(&report_json(&score, &[])).unwrap();
        assert!(text.starts_with('{'), "stdout must be one document: {text}");
        let parsed: Score = serde_json::from_str(&text).expect("report must parse as a baseline");
        assert_eq!(parsed, score);
    }

    fn metric(ingest_id: u64, payload: serde_json::Value) -> TaskEvent {
        TaskEvent {
            attempt_id: "att".into(),
            sequence: ingest_id,
            r#type: EventType::Metric,
            payload,
            created_at: "2026-01-01T00:00:00Z".into(),
            ingest_id,
        }
    }

    #[test]
    fn usage_sums_increments_and_ignores_everything_else() {
        let events = vec![
            metric(1, serde_json::json!({ "tokens": 100, "cost_cents": 4 })),
            metric(2, serde_json::json!({ "tokens": 50, "cost_cents": 1 })),
            // A context gauge, not an increment: counting it would double-count.
            metric(3, serde_json::json!({ "used": 9000 })),
            // Non-metric events never carry billing, even with the right keys.
            TaskEvent {
                r#type: EventType::Stdout,
                ..metric(4, serde_json::json!({ "tokens": 700, "cost_cents": 70 }))
            },
        ];
        assert_eq!(sum_usage(&events), (150, 5));
        assert_eq!(sum_usage(&[]), (0, 0));
    }
}
