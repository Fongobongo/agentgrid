//! Clap argument surface. Handlers live in [`crate::commands`].

use super::*;

#[derive(Parser)]
#[command(name = "ag", version, about = "agentgrid CLI")]
pub(crate) struct Cli {
    /// Control plane base URL (also AGENTGRID_SERVER).
    #[arg(
        long,
        env = "AGENTGRID_SERVER",
        default_value = "http://127.0.0.1:7800"
    )]
    pub(crate) server: String,
    /// Emit raw JSON instead of human-readable tables (machine-readable output).
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: AgCommand,
}

#[derive(Subcommand)]
pub(crate) enum AgCommand {
    /// Create a task.
    Run(RunArgs),
    /// Stream a task's events.
    Logs(LogsArgs),
    /// Show a task's status/result.
    Show(ShowArgs),
    /// Manage nodes (list / install over SSH).
    Nodes(nodes::NodeArgs),
    /// Cancel a task (queued -> cancelled; running -> ask node to stop).
    Cancel(CancelArgs),
    /// Retry a failed or cancelled task (back to queued).
    Retry(RetryArgs),
    /// Node enrollment tokens.
    Token(TokenArgs),
    /// Manage repositories.
    Repo(RepoArgs),
    /// Log in and store a session token for user-authenticated endpoints.
    Login(LoginArgs),
    /// Review and answer agent permission approvals (fail-closed by default).
    Approvals(registry::ApprovalArgs),
    /// Manage skill trust decisions (fail-closed: untrusted until trusted).
    Skills(registry::SkillsArgs),
    /// Manage MCP server registry (Stage 13 stdio servers a profile attaches).
    Mcp(registry::McpArgs),
    /// Manage agent profiles (system prompt + autonomy + limits; immutable revisions).
    Profiles(registry::ProfilesArgs),
    /// Live view of nodes + recent tasks (repeats every N seconds).
    Watch {
        /// Refresh interval in seconds.
        #[arg(short, long, default_value = "3")]
        interval: u64,
        /// How many recent tasks to show.
        #[arg(long, default_value = "12")]
        tasks: usize,
    },
    /// Manage the CP-managed egress proxy pool pushed to nodes.
    Proxy(registry::ProxyArgs),
    /// Run a fixture of eval cases and report pass rate / tokens / cost.
    Eval(eval::EvalArgs),
    /// CP-managed adapter env (custom endpoints/keys, pushed to nodes).
    AdapterEnv(registry::AdapterEnvArgs),
    /// Start the control plane (standalone binary).
    Server(ServerStartArgs),
    /// Control-plane HA: status, promote, demote, snapshot.
    Cluster(cluster::ClusterArgs),
    /// Define and run Agentgrid workflows (DAGs of agent steps).
    Workflow(workflow::WorkflowArgs),
    /// One-button project assembly: goal in, architect-planned multi-host
    /// build with collection and optional GitHub publish out.
    Assemble(workflow::AssembleArgs),
    /// Full-screen TUI dashboard (read-only monitoring).
    Tui(TuiArgs),
    /// One-screen overview: server health, nodes, tasks and workflow runs.
    Status,
    /// Print a shell completion script (bash/zsh/fish/elvish/powershell).
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Storage maintenance (artifact GC, disk status).
    Storage(StorageArgs),
    /// Plan 1.3: full-text search over tasks (FTS5).
    Search(SearchArgs),
    /// Plan 2.6 (#22c): overnight autopilot — loop run → validate → commit
    /// per iteration, roll back on fail, write agentgrid-workspace/<slug>/SUMMARY.md.
    Autopilot(AutopilotArgs),
    /// Plan 2.7 (#25): guided setup wizard.
    Setup(SetupArgs),
    /// Plan 2.7 (#25): diagnostic status (server, credentials, endpoints).
    Doctor(DoctorArgs),
    /// Plan 2.8 (#19): per-repo learnings (`ag learn list/add/approve/remove`).
    Learn(registry::LearnArgs),
    /// Plan 1.3: resume a past attempt as a new attempt with inherited context.
    Resume(ResumeArgs),
    /// Plan 1.3: add/remove/list task tags.
    Tag(TagArgs),
    /// Plan 1.4: GitHub issues as tasks (#2b) via the `gh` CLI.
    Issue(IssueArgs),
    /// Plan 1.12: read/write shared context notes for a task group (#7).
    Ctx(CtxArgs),
    /// Plan 2.1: manage org agents (identity, role, budget, heartbeats) (#18).
    Agent(registry::AgentArgs),
    /// Feature "opencode profiles": CP-hosted opencode configuration — list,
    /// show, set, delete; assign a profile to a node.
    Opencode(registry::OpencodeArgs),
    /// Plan 1.13: offline ctags-like extraction of top-level symbols/imports
    /// for a repo, intended as a system-prompt context packet for agents
    /// without built-in codebase awareness.
    Index(IndexArgs),
    /// Competitor-gap feature (project brain): generate/refresh a persistent
    /// AGENTS-BRAIN.md from a repository's task history — the file every
    /// attempt then reads as project memory.
    Brain(BrainArgs),
    /// Competitor-gap feature (consensus patch review, nitpicker-inspired):
    /// N reviewer adapters judge one task's changes.patch; unanimous APPROVE
    /// auto-approves the pending patch review, disagreement leaves it for a
    /// human.
    Review(ReviewArgs),
}

#[derive(Args)]
pub(crate) struct DoctorArgs {
    #[command(subcommand)]
    pub(crate) command: Option<DoctorSubcommand>,
}

#[derive(Subcommand)]
pub(crate) enum DoctorSubcommand {
    /// Inspect local control-plane runtime files, disk, and stale temp dirs.
    Runtime,
}

#[derive(Args)]
pub(crate) struct StorageArgs {
    #[command(subcommand)]
    pub(crate) command: StorageSub,
}

/// Plan 1.3: FTS5 search over task prompt/repository; `--events` switches
/// to full-text search over past task events.
#[derive(Args)]
pub(crate) struct SearchArgs {
    /// Query text (words to match).
    pub(crate) query: String,

    /// Search task events (agent logs/output) instead of task prompts.
    #[arg(long)]
    pub(crate) events: bool,
}

/// Competitor-gap feature (project brain): `ag brain <repo> [--out FILE]
/// [--limit N]` rebuilds AGENTS-BRAIN.md from a repository's task history.
#[derive(Args)]
pub(crate) struct BrainArgs {
    /// Repository name the brain is generated for.
    pub(crate) repository: String,
    /// Output file (default: AGENTS-BRAIN.md in the current directory).
    #[arg(long, default_value = "AGENTS-BRAIN.md")]
    pub(crate) out: String,
    /// Cap on tasks pulled for the digest (default 50).
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: u64,
}

/// Competitor-gap feature (consensus patch review, nitpicker-inspired):
/// `ag review <task> --models a,b` fires one review task per adapter over
/// the task's changes.patch. Verdicts come from each reviewer's `result`
/// event text; unanimous APPROVE auto-approves the pending patch review,
/// any REJECT/unclear verdict leaves it for a human.
#[derive(Args)]
pub(crate) struct ReviewArgs {
    /// Task id whose changes.patch is reviewed.
    pub(crate) task: String,
    /// Comma-separated reviewer adapters (>= 2), one review task each.
    #[arg(long, value_delimiter = ',')]
    pub(crate) models: Vec<String>,
}

/// Plan 1.3: resume an attempt — new attempt inheriting the task's prompt.
#[derive(Args)]
pub(crate) struct ResumeArgs {
    /// Attempt id to resume.
    pub(crate) attempt_id: String,
}

/// Plan 1.3: task tag management.
#[derive(Args)]
pub(crate) struct TagArgs {
    #[command(subcommand)]
    pub(crate) command: TagSub,
}

#[derive(Subcommand)]
pub(crate) enum TagSub {
    /// Add a tag to a task.
    Add { task_id: String, tag: String },
    /// Remove a tag from a task.
    Remove { task_id: String, tag: String },
    /// List a task's tags.
    List { task_id: String },
}

/// Plan 1.4 (#2b): issue-as-task via `gh`.
#[derive(Args)]
pub(crate) struct IssueArgs {
    #[command(subcommand)]
    pub(crate) command: IssueSub,
}

/// Plan 1.12 (#7): shared context notes for a task group.
#[derive(Args)]
pub(crate) struct CtxArgs {
    #[command(subcommand)]
    pub(crate) command: CtxSub,
}

#[derive(Subcommand)]
pub(crate) enum CtxSub {
    /// Set (or overwrite) one note: `ag ctx set <group> <key> <value>`.
    Set {
        group: String,
        key: String,
        value: String,
    },
    /// Read one note's value: `ag ctx get <group> <key>`.
    Get { group: String, key: String },
    /// List all notes for a group.
    Ls { group: String },
    /// Delete one note.
    Del { group: String, key: String },
}

/// Plan 2.6 (#22c): overnight autopilot driver.
#[derive(Args)]
pub(crate) struct AutopilotArgs {
    /// Objective (free text, rendered into every iteration prompt).
    pub(crate) objective: String,
    /// Repository name the task runs against (must be registered).
    #[arg(long)]
    pub(crate) repository: String,
    /// Adapter to drive (default mock).
    #[arg(long, default_value = "mock")]
    pub(crate) adapter: String,
    /// Validation command every iteration must exit 0 against.
    #[arg(long)]
    pub(crate) validate: Option<String>,
    /// Local checkout the node lands diffs into; iterations commit and roll
    /// back against this path. Required — the loop refuses without it.
    #[arg(long)]
    pub(crate) local_path: String,
    /// Max iterations before stopping (default 3).
    #[arg(long = "max-iterations", default_value_t = 3)]
    pub(crate) max_iterations: u32,
    /// Wall-clock ceiling in seconds (default 28800 = 8h).
    #[arg(long = "max-duration", default_value_t = 28800)]
    pub(crate) max_duration: u64,
    /// Directory under which `<slug>/SUMMARY.md` is written (default:
    /// `agentgrid-workspace` under the current working dir).
    #[arg(long = "summary-root")]
    pub(crate) summary_root: Option<String>,
}

/// Plan 2.7 (#25): wizard. `ag setup --accept-defaults` is non-interactive
/// (CI check); omit it for guided prompts. The wizard writes a session token
/// via the existing `save_token` flow and runs `doctor` checks at the end so
/// the operator walks away with a verified install.
#[derive(Args)]
pub(crate) struct SetupArgs {
    /// Non-interactive: skip every prompt and use defaults. Suitable for
    /// smoke-test setups on CI / in scripts.
    #[arg(long = "accept-defaults")]
    pub(crate) accept_defaults: bool,
    /// Username for the first user login (default `admin`).
    #[arg(long, default_value = "admin")]
    pub(crate) username: String,
    /// Password; omit (or pass `-`) to read from stdin. Recommended: leave
    /// unset so shells don't record the value.
    #[arg(long)]
    pub(crate) password: Option<String>,
    /// Adapter to register as the default for ad-hoc `ag run` calls
    /// (default `mock`).
    #[arg(long, default_value = "mock")]
    pub(crate) default_adapter: String,
    /// Skip the post-setup smoke task. With `--accept-defaults` the smoke
    /// task is also skipped unless `--smoke` is given explicitly.
    #[arg(long = "no-smoke")]
    pub(crate) no_smoke: bool,
}

#[derive(Subcommand)]
pub(crate) enum IssueSub {
    /// Create a task from a GitHub issue (`gh issue view <N>` under the hood).
    /// `[repo]` defaults to the current directory's GitHub repo. `--push` also
    /// pushes the agent branch, opens a PR and comments on the issue after a
    /// successful run (needs AGENTGRID_GITHUB_TOKEN on the node).
    Run {
        number: i64,
        repo: Option<String>,
        #[arg(long)]
        push: bool,
    },
    /// List open issues (`gh issue list`).
    Ls { repo: Option<String> },
    /// Show an issue's title/body (`gh issue view <N>`).
    Show { number: i64, repo: Option<String> },
}

#[derive(Subcommand)]
pub(crate) enum StorageSub {
    /// Reconcile the artifact tree against metadata. `--dry-run` only reports
    /// orphan files / dangling metadata without deleting anything.
    Gc {
        /// Report only; delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show free space on the control-plane artifact volume.
    Disk,
}

#[derive(Args)]
pub(crate) struct RunArgs {
    pub(crate) repository: String,
    pub(crate) prompt: String,
    #[arg(long, default_value = "mock")]
    pub(crate) adapter: String,
    #[arg(long)]
    pub(crate) node: Option<String>,
    /// Validation command run after the agent succeeds.
    #[arg(long)]
    pub(crate) validate: Option<String>,
    /// Per-task timeout in seconds.
    #[arg(long)]
    pub(crate) timeout: Option<u64>,
    /// Competitor-gap feature (task-level auto-retry): total attempts allowed
    /// (1 = no auto-retry). A failed attempt re-queues the task automatically
    /// until the budget is exhausted.
    #[arg(long, default_value_t = 1)]
    pub(crate) max_attempts: u32,
    /// Opt out of retrying after the node running the task disappears. By
    /// default a lost attempt spends the `--max-attempts` budget like any
    /// other failure, and the retry is placed on a *different* node. Pass
    /// this for tasks whose side effects must not be repeated — a lost
    /// attempt leaves no evidence of what the agent already did.
    #[arg(long)]
    pub(crate) no_retry_on_node_loss: bool,
    /// Plan 1.9 (#17): path to a `.agentgrid/workflows/*.yaml` file to run as
    /// a workflow. When `--workflow` is a directory, the first `*.yaml` inside
    /// it is used. Setting this ignores `prompt`/`adapter`.
    #[arg(long, conflicts_with = "prompt")]
    pub(crate) workflow: Option<String>,
    /// Plan 1.12 (#7): shared-context task group id — parallel runs in the
    /// same group share `ag ctx` notes and get `AG_GROUP_ID` on the node.
    #[arg(long)]
    pub(crate) group: Option<String>,
    /// Plan 2.9 (#20): fire `ag run` as a consensus vote. With `--consensus N`
    /// the CLI submits N identical tasks, one per adapter, each marked with
    /// the same consensus group id. The CP collapses the group when every
    /// member lands; disagreement produces a human-review approval.
    #[arg(long = "consensus", requires = "models")]
    pub(crate) consensus: Option<u32>,
    /// Plan 2.9 (#20): comma-separated adapter names for the vote. Length
    /// must equal `--consensus`; a short list is a CONF (see
    /// cmd_run for the exact validation).
    #[arg(long = "models", value_delimiter = ',')]
    pub(crate) models: Option<Vec<String>>,
    /// Feature "opencode profiles": per-task model override, merged over
    /// whatever profile the node has applied. Only forwarded when the
    /// assigned adapter starts with "opencode".
    #[arg(long = "opencode-model")]
    pub(crate) opencode_model: Option<String>,
    /// Feature "opencode profiles": per-task small-model override.
    #[arg(long = "opencode-small-model")]
    pub(crate) opencode_small_model: Option<String>,
    /// Security profile for the task (e.g. "default-strict"). A name ending
    /// in "-strict" only assigns to nodes with structured permission
    /// interception (Stage 12 Secure profile) — wrapper-only nodes report
    /// the task as ineligible instead of running it.
    #[arg(long = "security-profile")]
    pub(crate) security_profile: Option<String>,
    /// Push the successful attempt's branch to GitHub (needs --github-repo).
    #[arg(long = "github-push")]
    pub(crate) github_push: bool,
    /// GitHub repository `owner/name` for write-back.
    #[arg(long = "github-repo")]
    pub(crate) github_repo: Option<String>,
    /// Create the remote repository via the API when it does not exist yet
    /// (private; needs --github-push). A plain-dir worktree is `git init`-ed
    /// first — project assembly from scratch.
    #[arg(long = "github-create")]
    pub(crate) github_create: bool,
    /// Per-task token ceiling: kill the attempt with `budget_exceeded` when
    /// reported adapter usage passes it (autopilot safety).
    #[arg(long = "max-tokens")]
    pub(crate) max_tokens: Option<u64>,
    /// Per-task cost ceiling in cents (same enforcement as --max-tokens).
    #[arg(long = "max-cost-cents")]
    pub(crate) max_cost_cents: Option<u64>,
}

#[derive(Args)]
pub(crate) struct ServerStartArgs {
    /// Listen address (sets AGENTGRID_LISTEN).
    #[arg(long, default_value = "127.0.0.1:7800")]
    pub(crate) listen: String,
    /// SQLite database path (sets AGENTGRID_DB).
    #[arg(long, default_value = "control-plane.db")]
    pub(crate) db: String,
    /// TLS certificate (PEM). Enables HTTPS on the control plane.
    #[arg(long)]
    pub(crate) tls_cert: Option<String>,
    /// TLS private key (PEM). Enables HTTPS on the control plane.
    #[arg(long)]
    pub(crate) tls_key: Option<String>,
}

#[derive(Args)]
pub(crate) struct LogsArgs {
    pub(crate) task_id: String,
    /// Follow until the task reaches a terminal state.
    #[arg(long)]
    pub(crate) follow: bool,
    /// Disable colored output. Default: color on.
    #[arg(long)]
    pub(crate) no_color: bool,
}

#[derive(Args)]
pub(crate) struct ShowArgs {
    pub(crate) task_id: String,
    /// Hardening P2 item 37: show scheduler eligibility reasoning for the task
    /// even when it is no longer queued (e.g. after assignment), mirroring
    /// `ag task explain`.
    #[arg(long)]
    pub(crate) explain: bool,
}
#[derive(Args)]
pub(crate) struct CancelArgs {
    pub(crate) task_id: String,
}
#[derive(Args)]
pub(crate) struct RetryArgs {
    pub(crate) task_id: String,
}
#[derive(Args)]
pub(crate) struct LoginArgs {
    pub(crate) username: String,
    /// Password. Omit (or pass `-`) to be prompted via stdin instead of
    /// putting the secret in shell history / `ps`.
    pub(crate) password: Option<String>,
}
#[derive(Args)]
pub(crate) struct TokenArgs {
    #[command(subcommand)]
    pub(crate) action: TokenAction,
}
#[derive(Subcommand)]
pub(crate) enum TokenAction {
    /// Issue a one-time enrollment token for a new node.
    Create,
}
#[derive(Args)]
pub(crate) struct RepoArgs {
    #[command(subcommand)]
    pub(crate) action: RepoAction,
}
#[derive(Subcommand)]
pub(crate) enum RepoAction {
    /// Register a repository.
    Add(RepoAddArgs),
}
#[derive(Args)]
pub(crate) struct RepoAddArgs {
    pub(crate) name: String,
    /// Git URL (https/token or local path).
    pub(crate) git_url: String,
    /// Default branch new attempts branch from.
    #[arg(long, default_value = "main")]
    pub(crate) branch: String,
    /// Optional validation command run after the agent succeeds.
    #[arg(long)]
    pub(crate) validate: Option<String>,
    /// Plan 6.9: structured requirements as a JSON object
    /// (`{"memory_mb":2048,"tools":[{"name":"git","version_req":">=2.39"}]}`).
    /// Rejected client-side when it is not valid JSON.
    #[arg(long)]
    pub(crate) requirements: Option<String>,
}
#[derive(Args)]
pub(crate) struct TuiArgs {
    /// Disable colored output. Default: color on.
    #[arg(long)]
    pub(crate) no_color: bool,
}
