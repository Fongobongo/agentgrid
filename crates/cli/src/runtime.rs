//! `ag doctor runtime` — local control-plane runtime inspection.
//!
//! Answers the questions an operator asks after the "deleted temp dir while
//! the process was running" incident: where does the control plane think its
//! database is, does the instance lock still match the live process, how big
//! is the WAL, is the disk filling up, and are there processes still holding
//! deleted files (the thing that made a 703 MiB log invisible to `du`).
//!
//! Everything here is read-only and local: it inspects this host, not the
//! server. The server-side checks stay in `ag doctor` (HTTP surface).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// One check result. `Fail` makes the command exit non-zero.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|c| c.status == Status::Fail)
    }
}

/// Stale scratch directories older than this are reported as warnings.
const STALE_TMP_AGE: Duration = Duration::from_secs(24 * 3600);
/// Below this much free space the disk is a problem worth failing on.
const FREE_BYTES_FAIL: u64 = 1024 * 1024 * 1024;

pub fn collect() -> Report {
    let mut checks = Vec::new();

    let runtime = match agentgrid_common::RuntimeConfig::from_env() {
        Ok(r) => r,
        Err(e) => {
            checks.push(Check::new(
                "config",
                Status::Fail,
                format!("runtime config invalid: {e}"),
            ));
            return Report { checks };
        }
    };
    let db = runtime.control_plane.database.clone();
    let lock = lock_path(&db);

    // 1. Database file.
    match std::fs::metadata(&db) {
        Ok(md) => checks.push(Check::new(
            "database",
            Status::Ok,
            format!("{} ({} bytes)", db.display(), md.len()),
        )),
        Err(e) => checks.push(Check::new(
            "database",
            Status::Fail,
            format!("{}: {e}", db.display()),
        )),
    }

    // 2. WAL size — a WAL that never gets truncated is disk that never comes
    //    back.
    let wal = wal_path(&db);
    match std::fs::metadata(&wal) {
        Ok(md) => {
            let status = if md.len() > 256 * 1024 * 1024 {
                Status::Warn
            } else {
                Status::Ok
            };
            checks.push(Check::new(
                "wal",
                status,
                format!("{} ({} bytes)", wal.display(), md.len()),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(Check::new("wal", Status::Ok, "absent (checkpointed)"))
        }
        Err(e) => checks.push(Check::new(
            "wal",
            Status::Warn,
            format!("{}: {e}", wal.display()),
        )),
    }

    // 3. Instance lock: the file must exist, and the live control plane must
    //    still hold THAT inode. A replaced lock file means the flock protects
    //    an unlinked inode — two control planes could both "hold the lock".
    match std::fs::metadata(&lock) {
        Ok(md) => {
            let inode = inode_of(&md);
            let holders = lock_holders(&lock);
            let status = if holders.is_empty() {
                Status::Warn
            } else {
                Status::Ok
            };
            checks.push(Check::new(
                "instance-lock",
                status,
                format!(
                    "{} inode {inode}, held by {}",
                    lock.display(),
                    if holders.is_empty() {
                        "no live process".to_string()
                    } else {
                        holders
                            .iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
            ));
        }
        Err(e) => checks.push(Check::new(
            "instance-lock",
            Status::Warn,
            format!("{}: {e}", lock.display()),
        )),
    }

    // 4. Free disk space on the database volume. `parent()` of a bare file
    //    name is an empty path, which statvfs rejects — use "." there.
    let db_dir = match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match free_bytes(db_dir) {
        Some(free) => {
            let status = if free < FREE_BYTES_FAIL {
                Status::Fail
            } else {
                Status::Ok
            };
            checks.push(Check::new(
                "disk-free",
                status,
                format!("{} MiB free", free / (1024 * 1024)),
            ));
        }
        None => checks.push(Check::new(
            "disk-free",
            Status::Warn,
            "statvfs unavailable on this platform",
        )),
    }

    // 5. Deleted-open files: a process holding a deleted log/database keeps
    //    its bytes alive while `du` reports them gone. Only files in this
    //    runtime's own directory count: an unrelated `.so` or rotated log in
    //    another agentgrid-named process is not this incident.
    let owned = vec![db.clone(), wal.clone(), lock.clone(), db_dir.to_path_buf()];
    let deleted = deleted_open_files(&owned);
    let status = if deleted.is_empty() {
        Status::Ok
    } else {
        Status::Fail
    };
    checks.push(Check::new(
        "deleted-open-files",
        status,
        if deleted.is_empty() {
            "none".to_string()
        } else {
            deleted
                .iter()
                .map(|(pid, path)| format!("pid {pid}: {path}"))
                .collect::<Vec<_>>()
                .join("; ")
        },
    ));

    // 6. Stale scratch dirs — the exact shape of the incident: a control
    //    plane (or node) started under /tmp and forgotten there.
    let stale = stale_temp_dirs(&std::env::temp_dir(), STALE_TMP_AGE);
    let status = if stale.is_empty() {
        Status::Ok
    } else {
        Status::Warn
    };
    checks.push(Check::new(
        "stale-temp-dirs",
        status,
        if stale.is_empty() {
            "none".to_string()
        } else {
            stale
                .iter()
                .map(|(p, age)| format!("{} ({}h old)", p.display(), age / 3600))
                .collect::<Vec<_>>()
                .join("; ")
        },
    ));

    Report { checks }
}

pub fn lock_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

pub fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

#[cfg(unix)]
fn inode_of(md: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", md.dev(), md.ino())
}

#[cfg(not(unix))]
fn inode_of(_md: &std::fs::Metadata) -> String {
    "unknown".into()
}

/// PIDs holding an open descriptor on `path` (the flock holder on Linux).
#[cfg(target_os = "linux")]
fn lock_holders(path: &Path) -> Vec<u32> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !String::from_utf8_lossy(&cmdline).contains("agentgrid") {
            continue;
        }
        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target_link) = std::fs::read_link(fd.path()) {
                if target_link == target {
                    out.push(pid);
                    break;
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn lock_holders(_path: &Path) -> Vec<u32> {
    Vec::new()
}

/// Open file descriptors whose target no longer exists (Linux: the link ends
/// with " (deleted)"), limited to agentgrid processes. Unrelated deleted FDs
/// from editors or package managers are not this daemon's incident and must
/// not turn the runtime doctor red.
#[cfg(target_os = "linux")]
fn deleted_open_files(owned: &[PathBuf]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&cmdline);
        if !cmd.contains("agentgrid") {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                let s = link.to_string_lossy();
                if is_owned_deleted_target(&s, owned) {
                    out.push((pid, s.into_owned()));
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn deleted_open_files(_owned: &[PathBuf]) -> Vec<(u32, String)> {
    Vec::new()
}

/// Only deleted files from this runtime's own directory are a finding. A
/// rotated log or an upgraded shared library in some other agentgrid process
/// must not turn `ag doctor runtime` red.
fn is_owned_deleted_target(link: &str, owned: &[PathBuf]) -> bool {
    let Some(path) = link.strip_suffix(" (deleted)") else {
        return false;
    };
    let path = Path::new(path);
    owned.iter().any(|p| path.starts_with(p))
}

fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path; `s` is a zeroed statvfs.
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
        if rc != 0 {
            return None;
        }
        Some(s.f_bavail as u64 * s.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Scratch directories that look like ours (`ag-*`, `agentgrid-*`) and have
/// not been touched for `max_age`.
pub fn stale_temp_dirs(root: &Path, max_age: Duration) -> Vec<(PathBuf, u64)> {
    let now = SystemTime::now();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with("ag-") || name.starts_with("agentgrid-")) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if !md.is_dir() {
            continue;
        }
        let Ok(modified) = md.modified() else {
            continue;
        };
        let age = now
            .duration_since(modified)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        if age >= max_age.as_secs() {
            out.push((e.path(), age));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_deleted_files_from_the_runtime_directory_are_reported() {
        let owned = vec![PathBuf::from("/var/lib/agentgrid")];
        assert!(is_owned_deleted_target(
            "/var/lib/agentgrid/control-plane.db (deleted)",
            &owned
        ));
        assert!(is_owned_deleted_target(
            "/var/lib/agentgrid/cp.log (deleted)",
            &owned
        ));
        assert!(!is_owned_deleted_target(
            "/var/lib/agentgrid/control-plane.db",
            &owned
        ));
        assert!(!is_owned_deleted_target(
            "/var/lib/agentgrid-evil/cp.log (deleted)",
            &owned
        ));
        assert!(!is_owned_deleted_target(
            "/usr/lib/libsomething.so (deleted)",
            &owned
        ));
    }

    #[test]
    fn stale_temp_dirs_skips_non_agentgrid_dirs() {
        let root = std::env::temp_dir().join(format!("ag-doctor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("ag-old")).unwrap();
        std::fs::create_dir_all(root.join("agentgrid-old")).unwrap();
        std::fs::create_dir_all(root.join("other-old")).unwrap();

        // Zero age: everything is stale, so only the name filter can drop entries.
        let stale = stale_temp_dirs(&root, Duration::ZERO);
        let names: Vec<_> = stale
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["ag-old".to_string(), "agentgrid-old".to_string()]
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
