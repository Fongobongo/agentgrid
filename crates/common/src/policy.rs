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

/// Heuristic builtin provider (Stage 9 foundation).
pub struct BuiltinPolicyProvider;

impl BuiltinPolicyProvider {
    pub fn new() -> Self {
        BuiltinPolicyProvider
    }

    /// Default decision matrix: dangerous classes are denied/asked, benign
    /// classes allowed. Plug in a stricter provider for `strict` profiles.
    fn decide(class: RiskClass) -> PolicyDecision {
        match class {
            RiskClass::Read => PolicyDecision::Allow,
            RiskClass::EditWorkspace => PolicyDecision::Allow,
            RiskClass::ExecuteLocal => PolicyDecision::Allow,
            RiskClass::NetworkWrite => PolicyDecision::Ask,
            RiskClass::GitRemote => PolicyDecision::Ask,
            RiskClass::PackageInstall => PolicyDecision::Ask,
            RiskClass::Destructive => PolicyDecision::Deny,
        }
    }
}

impl Default for BuiltinPolicyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPolicyProvider for BuiltinPolicyProvider {
    fn evaluate(&self, command: &str, _cwd: &str) -> Result<PolicyVerdict, PolicyError> {
        // Tokenize respecting simple single/double quotes so `rm -rf "a b"`
        // is one argument. An unterminated quote is a parse failure → ask.
        let tokens = match tokenize(command) {
            Some(t) => t,
            None => {
                return Ok(PolicyVerdict::fail_closed("command tokenization failed"));
            }
        };
        if tokens.is_empty() {
            return Ok(PolicyVerdict::fail_closed("empty command"));
        }
        let class = classify(&tokens);
        let decision = Self::decide(class);
        let reason = format!("builtin classifier → {class:?}");
        Ok(PolicyVerdict {
            decision,
            risk_class: class,
            reason,
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
    if head == "rm"
        && tokens
            .iter()
            .any(|t| t == "-rf" || t == "-fr" || t == "-r" || t == "-f")
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
    ) || (head == "npm" && tokens.get(1).map(|s| s.as_str()) == Some("install"))
        || (head == "cargo" && tokens.get(1).map(|s| s.as_str()) == Some("install"))
        || (head == "pip" && tokens.get(1).map(|s| s.as_str()) == Some("install"))
    {
        return RiskClass::PackageInstall;
    }

    // Git remote operations touch a remote ref.
    if head == "git" {
        if let Some(sub) = tokens.get(1).map(|s| s.as_str()) {
            match sub {
                "push" | "fetch" | "pull" | "clone" | "submodule" | "remote" => {
                    return RiskClass::GitRemote
                }
                "commit" | "add" | "checkout" | "merge" | "rebase" | "status" | "diff" | "log"
                | "show" | "reset" | "branch" | "mv" | "rm" => return RiskClass::EditWorkspace,
                _ => return RiskClass::EditWorkspace,
            }
        }
        return RiskClass::EditWorkspace;
    }

    // Network write to a peer.
    if (head == "curl" && joined.contains("-X POST"))
        || (head == "curl" && (joined.contains("ftp") || joined.contains("--upload")))
        || matches!(head, "scp" | "rsync" | "sftp" | "wget" | "ftp")
    {
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

    // Plain reads / inspection.
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
            | "echo"
            | "wc"
            | "sort"
            | "uniq"
            | "diff"
            | "file"
            | "stat"
            | "date"
            | "git"
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
}
