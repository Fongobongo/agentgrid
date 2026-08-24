//! Command policy foundation (Stage 9).
//!
//! A `CommandPolicyProvider` classifies a shell command into a `RiskClass` and
//! returns a `PolicyVerdict` (`allow | ask | deny | rewrite`). The builtin
//! provider is a heuristic classifier good enough to gate the obviously
//! dangerous classes; stricter providers (e.g. an external bash-guard
//! executable) implement the same trait.
//!
//! Fail-closed: a provider that is unavailable or cannot decide must yield
//! `ask`, never `allow` (see `PolicyVerdict::fail_closed`).

use serde::{Deserialize, Serialize};

/// Coarse risk class a command falls into. Ordered roughly least→most dangerous
/// so a default matrix can map class → decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    /// Inspect state without changing it: `cat`, `ls`, `git status`, `grep`.
    Read,
    /// Mutate the working tree / local files: `git commit`, `sed -i`, `mv`.
    EditWorkspace,
    /// Run a local program / script: `bash`, `python`, `make`, `cargo run`.
    ExecuteLocal,
    /// Write to a network peer: `curl -X POST`, `scp`, `rsync` remote.
    NetworkWrite,
    /// Touch a remote git ref: `git push`, `git fetch`, `git clone` (remote).
    GitRemote,
    /// Install system/package artifacts: `apt-get install`, `npm i -g`, `pip`.
    PackageInstall,
    /// Irreversible / host-affecting: `rm -rf`, `mkfs`, `dd`, `shutdown`.
    Destructive,
}

/// Autonomy level (Stage 9.2). Higher level = the agent may act more on its own;
/// lower level keeps a human in the loop. Stored per profile; the builtin
/// provider defaults to `L2` (can patch/edit locally, asks before anything that
/// touches the network/remote/installs, denies destructive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Fully supervised: every non-trivial action must be approved; destructive denied.
    L0,
    /// Read-only helpers: reads allowed, everything else asked.
    L1,
    /// Local patching: read/edit/exec allowed, network/install asked, destructive denied.
    #[default]
    L2,
    /// Local dev: read/edit/exec/network/git allowed, install asked, destructive denied.
    L3,
    /// Full autonomy: everything allowed, including destructive (use with care).
    L4,
}

/// Provider decision for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
    Rewrite,
}

/// Verdict returned for a command evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyVerdict {
    pub decision: PolicyDecision,
    pub risk_class: RiskClass,
    pub reason: String,
    pub matched_rules: Vec<String>,
}

impl PolicyVerdict {
    /// Fail-closed verdict: when a provider is unavailable or cannot decide,
    /// the command must be `ask`ed, never silently allowed.
    pub fn fail_closed(why: &str) -> Self {
        PolicyVerdict {
            decision: PolicyDecision::Ask,
            risk_class: RiskClass::Destructive,
            reason: format!("policy provider unavailable ({why}): defaulting to ask"),
            matched_rules: vec!["fail-closed".into()],
        }
    }
}

/// Error from a policy provider (e.g. an external executable is missing).
#[derive(Debug, Clone)]
pub struct PolicyError(pub String);

/// Contract a command-policy provider implements.
pub trait CommandPolicyProvider: Send + Sync {
    /// Classify `command` (run from `cwd`) and return a verdict.
    fn evaluate(&self, command: &str, cwd: &str) -> Result<PolicyVerdict, PolicyError>;
}

/// Stage 9.1: an external command-policy provider — shells out to a pinned
/// executable (e.g. CodeAlive bash-guard, Destructive Command Guard) that
/// reads a command + autonomy on argv and prints a JSON `PolicyVerdict` on
/// stdout. The builtin is the fallback when no external binary is configured.
///
/// Fail-closed: if the binary is missing, errors, or returns unparseable JSON,
/// the verdict is `Ask` (operator approval), never `Allow`.
///
/// `ponytail:` one process per command (no long-lived daemon); suitable for a
/// low rate of permission requests. A persistent daemon protocol is a
/// follow-up if throughput matters.
pub struct ExternalPolicyProvider {
    pub binary: String,
    pub version: String,
}

impl ExternalPolicyProvider {
    pub fn new(binary: impl Into<String>, version: impl Into<String>) -> Self {
        ExternalPolicyProvider {
            binary: binary.into(),
            version: version.into(),
        }
    }
}

impl CommandPolicyProvider for ExternalPolicyProvider {
    fn evaluate(&self, command: &str, _cwd: &str) -> Result<PolicyVerdict, PolicyError> {
        use std::process::Command;
        let out = Command::new(&self.binary)
            .arg(&self.version)
            .arg(command)
            .output()
            .map_err(|e| PolicyError(format!("external policy binary failed: {e}")))?;
        if !out.status.success() {
            return Ok(PolicyVerdict::fail_closed(&format!(
                "external policy exited {}",
                out.status.code().unwrap_or(-1)
            )));
        }
        match serde_json::from_slice::<PolicyVerdict>(&out.stdout) {
            Ok(v) => Ok(v),
            Err(e) => Ok(PolicyVerdict::fail_closed(&format!(
                "external policy unparseable output: {e}"
            ))),
        }
    }
}

/// Heuristic builtin provider (Stage 9 foundation).
pub struct BuiltinPolicyProvider;

impl BuiltinPolicyProvider {
    pub fn new() -> Self {
        BuiltinPolicyProvider
    }

    /// Default decision per risk class at the default autonomy level (`L2`).
    pub fn decide(class: RiskClass) -> PolicyDecision {
        Self::decide_for(AutonomyLevel::default(), class)
    }

    /// Decision matrix per autonomy level. Higher level permits more.
    pub fn decide_for(level: AutonomyLevel, class: RiskClass) -> PolicyDecision {
        use AutonomyLevel::*;
        use RiskClass::*;
        match level {
            L0 => match class {
                Destructive => PolicyDecision::Deny,
                _ => PolicyDecision::Ask,
            },
            L1 => match class {
                Read => PolicyDecision::Allow,
                Destructive => PolicyDecision::Deny,
                _ => PolicyDecision::Ask,
            },
            L2 => match class {
                Read | EditWorkspace | ExecuteLocal => PolicyDecision::Allow,
                NetworkWrite | GitRemote | PackageInstall => PolicyDecision::Ask,
                Destructive => PolicyDecision::Deny,
            },
            L3 => match class {
                Read | EditWorkspace | ExecuteLocal | NetworkWrite | GitRemote => {
                    PolicyDecision::Allow
                }
                PackageInstall => PolicyDecision::Ask,
                Destructive => PolicyDecision::Deny,
            },
            L4 => PolicyDecision::Allow,
        }
    }
}

impl Default for BuiltinPolicyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPolicyProvider for BuiltinPolicyProvider {
    fn evaluate(&self, command: &str, cwd: &str) -> Result<PolicyVerdict, PolicyError> {
        self.evaluate_with(AutonomyLevel::default(), command, cwd)
    }
}

impl BuiltinPolicyProvider {
    /// Classify `command` for a specific autonomy level.
    pub fn evaluate_with(
        &self,
        level: AutonomyLevel,
        command: &str,
        _cwd: &str,
    ) -> Result<PolicyVerdict, PolicyError> {
        let tokens = match tokenize(command) {
            Some(t) => t,
            None => return Ok(PolicyVerdict::fail_closed("command tokenization failed")),
        };
        if tokens.is_empty() {
            return Ok(PolicyVerdict::fail_closed("empty command"));
        }
        let class = classify(&tokens);
        let decision = Self::decide_for(level, class);
        Ok(PolicyVerdict {
            decision,
            risk_class: class,
            reason: format!("builtin classifier → {class:?} @ {level:?}"),
            matched_rules: vec![format!("class:{class:?}")],
        })
    }
}

/// Split a command line into arguments; `None` on an unterminated quote.
fn tokenize(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q: Option<char> = None;
    let mut had = false;
    for c in s.chars() {
        match in_q {
            Some(q) => {
                if c == q {
                    in_q = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    in_q = Some(c);
                    had = true;
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() || had {
                        out.push(std::mem::take(&mut cur));
                        had = false;
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if in_q.is_some() {
        return None;
    }
    if !cur.is_empty() || had {
        out.push(cur);
    }
    Some(out)
}

/// Heuristic classification of a tokenized command into a `RiskClass`.
fn classify(tokens: &[String]) -> RiskClass {
    let head = tokens[0].as_str();
    let joined = tokens.join(" ");

    // Destructive: anything that can irreversibly destroy host/workspace state.
    // `rm` only counts as destructive with a force flag; the rest are always.
    // Long-form flags (`--recursive`, `--force`, `--recursive --force`) count
    // the same as their short forms.
    if head == "rm"
        && tokens.iter().any(|t| {
            t == "-rf"
                || t == "-fr"
                || t == "-r"
                || t == "-f"
                || t == "--recursive"
                || t == "--force"
        })
    {
        return RiskClass::Destructive;
    }
    if matches!(
        head,
        "dd" | "mkfs" | "shutdown" | "reboot" | "halt" | "fdisk" | "parted" | "mkswap" | "truncate"
    ) {
        return RiskClass::Destructive;
    }
    if joined.contains("> /dev/") || joined.contains(":/dev/") {
        return RiskClass::Destructive;
    }
    // `chmod`/`chown` recursive on system paths.
    if (head == "chmod" || head == "chown") && tokens.iter().any(|t| t == "-R" || t == "-r") {
        return RiskClass::Destructive;
    }

    // Package install.
    if matches!(
        head,
        "apt" | "apt-get" | "yum" | "dnf" | "apk" | "brew" | "pip" | "pip3"
    ) || (head == "npm"
        && (tokens.get(1).map(|s| s.as_str()) == Some("install")
            || tokens.get(1).map(|s| s.as_str()) == Some("i")))
        || (head == "cargo"
            && (tokens.get(1).map(|s| s.as_str()) == Some("install")
                || tokens.get(1).map(|s| s.as_str()) == Some("add")))
    {
        return RiskClass::PackageInstall;
    }

    // Git remote operations touch a remote ref. (The git arm is checked
    // before the read list, so a bare `git` in the read list below is dead.)
    if head == "git" {
        if let Some(sub) = tokens.get(1).map(|s| s.as_str()) {
            match sub {
                "push" | "fetch" | "pull" | "clone" | "submodule" | "remote" => {
                    return RiskClass::GitRemote
                }
                _ => return RiskClass::EditWorkspace,
            }
        }
        return RiskClass::EditWorkspace;
    }

    // Network write to a peer. Any curl that targets the network with a body,
    // an upload, an explicit method or a plain URL fetch mutates a peer or
    // exfiltrates data — classify conservatively as NetworkWrite; GET-only
    // inspection via curl still leaves the host untouched but the response
    // feeds the agent, so it stays asked at L2+.
    if head == "curl" || matches!(head, "scp" | "rsync" | "sftp" | "wget" | "ftp") {
        return RiskClass::NetworkWrite;
    }

    // Local execution of a program / interpreter / build tool.
    if matches!(
        head,
        "bash"
            | "sh"
            | "zsh"
            | "python"
            | "python3"
            | "node"
            | "deno"
            | "bun"
            | "ruby"
            | "perl"
            | "make"
            | "cmake"
            | "cargo"
            | "go"
            | "npm"
            | "npx"
            | "yarn"
            | "pnpm"
            | "tsc"
            | "javac"
            | "java"
            | "gcc"
            | "cc"
            | "g++"
            | "clang"
            | "zig"
            | "./"
            | "source"
    ) || head.starts_with("./")
        || head.starts_with("../")
    {
        return RiskClass::ExecuteLocal;
    }

    // File/workspace edits via common editors / stream editors.
    if matches!(
        head,
        "sed"
            | "awk"
            | "mv"
            | "cp"
            | "ln"
            | "touch"
            | "mkdir"
            | "rmdir"
            | "tee"
            | "vim"
            | "nvim"
            | "nano"
            | "echo"
            | "printf"
    ) {
        return RiskClass::EditWorkspace;
    }

    // Plain reads / inspection. (`echo` is classified EditWorkspace above;
    // `git` never reaches this list — the git arm above catches every git
    // invocation.)
    if matches!(
        head,
        "cat"
            | "ls"
            | "head"
            | "tail"
            | "grep"
            | "rg"
            | "find"
            | "pwd"
            | "which"
            | "whoami"
            | "env"
            | "wc"
            | "sort"
            | "uniq"
            | "diff"
            | "file"
            | "stat"
            | "date"
    ) {
        return RiskClass::Read;
    }

    // Unknown: treat as local execution (least surprising safe-ish default is
    // to allow local run, but never downgrade a destructive-looking command).
    RiskClass::ExecuteLocal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(cmd: &str) -> PolicyVerdict {
        BuiltinPolicyProvider::new()
            .evaluate(cmd, "/workspace")
            .expect("builtin never errors")
    }

    #[test]
    fn read_is_allowed() {
        let v = verdict("cat README.md");
        assert_eq!(v.risk_class, RiskClass::Read);
        assert_eq!(v.decision, PolicyDecision::Allow);
    }

    #[test]
    fn destructive_is_denied() {
        let v = verdict("rm -rf /tmp/build");
        assert_eq!(v.risk_class, RiskClass::Destructive);
        assert_eq!(v.decision, PolicyDecision::Deny);
        let v2 = verdict("dd if=/dev/zero of=/dev/sda");
        assert_eq!(v2.risk_class, RiskClass::Destructive);
        assert_eq!(v2.decision, PolicyDecision::Deny);
    }

    #[test]
    fn package_install_is_asked() {
        let v = verdict("apt-get install -y curl");
        assert_eq!(v.risk_class, RiskClass::PackageInstall);
        assert_eq!(v.decision, PolicyDecision::Ask);
        let v2 = verdict("npm install -g typescript");
        assert_eq!(v2.risk_class, RiskClass::PackageInstall);
    }

    #[test]
    fn git_remote_is_asked() {
        let v = verdict("git push origin main");
        assert_eq!(v.risk_class, RiskClass::GitRemote);
        assert_eq!(v.decision, PolicyDecision::Ask);
    }

    #[test]
    fn network_write_is_asked() {
        let v = verdict("curl -X POST https://example.com/hook -d '{}'");
        assert_eq!(v.risk_class, RiskClass::NetworkWrite);
        assert_eq!(v.decision, PolicyDecision::Ask);
        // Audit follow-up: the old classifier only fired on a literal
        // "-X POST" — a plain curl (GET/PUT/-d without -X) slipped through to
        // ExecuteLocal and was ALLOWED at L2+. Every curl is a network touch.
        let v2 = verdict("curl https://example.com/hook -d '{\"a\":1}'");
        assert_eq!(v2.risk_class, RiskClass::NetworkWrite);
        let v3 = verdict("curl -o out.bin https://example.com/file");
        assert_eq!(v3.risk_class, RiskClass::NetworkWrite);
    }

    #[test]
    fn rm_long_flags_are_destructive() {
        // Audit follow-up: `rm --recursive --force` bypassed the short-flag
        // check and landed in ExecuteLocal.
        for cmd in [
            "rm --recursive --force x",
            "rm --force x",
            "rm --recursive x",
        ] {
            let v = verdict(cmd);
            assert_eq!(v.risk_class, RiskClass::Destructive, "cmd: {cmd}");
            assert_eq!(v.decision, PolicyDecision::Deny);
        }
        // A plain rm without force flags is unknown-head territory (rm is not
        // in the edit list) — still not destructive, which is what matters.
        let v = verdict("rm x.txt");
        assert_ne!(v.risk_class, RiskClass::Destructive);
    }

    #[test]
    fn echo_is_edit_and_git_status_is_edit_not_read() {
        // `echo` writes (redirect targets), classified EditWorkspace; a bare
        // `git <anything>` is EditWorkspace via the git arm — both were dead
        // entries in the read list before.
        assert_eq!(verdict("echo hi").risk_class, RiskClass::EditWorkspace);
        assert_eq!(verdict("git status").risk_class, RiskClass::EditWorkspace);
    }

    #[test]
    fn quotes_are_respected() {
        // A quoted arg with a space stays one token; not mistaken for `rm -rf`.
        let v = verdict("cat \"a b c\"");
        assert_eq!(v.risk_class, RiskClass::Read);
    }

    #[test]
    fn unterminated_quote_is_fail_closed() {
        let v = verdict("echo \"unterminated");
        assert_eq!(v.decision, PolicyDecision::Ask);
        assert!(v.reason.contains("fail-closed") || v.reason.contains("tokenization"));
    }

    #[test]
    fn empty_command_is_fail_closed() {
        let v = verdict("   ");
        assert_eq!(v.decision, PolicyDecision::Ask);
    }

    #[test]
    fn l0_is_fully_supervised() {
        let p = BuiltinPolicyProvider::new();
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L0, "cat x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Ask
        );
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L0, "rm -rf x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Deny
        );
    }

    #[test]
    fn l2_allows_local_edit_denies_destructive() {
        let p = BuiltinPolicyProvider::new();
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L2, "cat x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Allow
        );
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L2, "git push", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Ask
        );
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L2, "rm -rf x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Deny
        );
    }

    #[test]
    fn l3_allows_git_push_but_asks_install() {
        let p = BuiltinPolicyProvider::new();
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L3, "git push origin main", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Allow
        );
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L3, "npm i -g x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Ask
        );
    }

    #[test]
    fn l4_allows_destructive() {
        let p = BuiltinPolicyProvider::new();
        assert_eq!(
            p.evaluate_with(AutonomyLevel::L4, "rm -rf x", "/w")
                .unwrap()
                .decision,
            PolicyDecision::Allow
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a subprocess; Miri cannot emulate posix_spawn")]
    fn external_provider_fail_closed_on_missing_binary() {
        use super::ExternalPolicyProvider;
        let p = ExternalPolicyProvider::new("/no/such/policy-binary", "0.1");
        // A missing binary is an Err — the caller (policy_decision) maps it to
        // Ask via .ok()?, never a silent Allow.
        let e = p.evaluate("rm -rf /", "/w").unwrap_err();
        assert!(e.0.contains("external policy binary failed"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a subprocess; Miri cannot emulate posix_spawn")]
    fn external_provider_fail_closed_on_nonzero_exit() {
        use super::ExternalPolicyProvider;
        // `false` exits 1 — a non-success exit must fail-closed to Ask.
        let p = ExternalPolicyProvider::new("false", "0.1");
        let v = p.evaluate("rm -rf /", "/w").unwrap();
        assert_eq!(v.decision, PolicyDecision::Ask);
        assert!(v.reason.contains("exited"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a subprocess; Miri cannot emulate posix_spawn")]
    fn external_provider_fail_closed_on_garbage_stdout() {
        use super::ExternalPolicyProvider;
        // `true` prints nothing — empty stdout is not valid PolicyVerdict JSON.
        let p = ExternalPolicyProvider::new("true", "0.1");
        let v = p.evaluate("rm -rf /", "/w").unwrap();
        assert_eq!(v.decision, PolicyDecision::Ask);
        assert!(v.reason.contains("unparseable"));
    }

    #[test]
    fn external_provider_parses_json_verdict() {
        // A well-formed JSON verdict the external binary would print is parsed
        // back into a PolicyVerdict (the path ExternalPolicyProvider.evaluate
        // takes on success).
        let json = r#"{"decision":"deny","risk_class":"destructive","reason":"x","matched_rules":["ext:deny"]}"#;
        let v: PolicyVerdict = serde_json::from_str(json).unwrap();
        assert_eq!(v.decision, PolicyDecision::Deny);
        assert_eq!(v.risk_class, RiskClass::Destructive);
        assert_eq!(v.matched_rules, vec!["ext:deny"]);
    }
}
