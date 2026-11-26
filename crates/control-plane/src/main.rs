use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentgrid_control_plane::ha::NodeRole;
use agentgrid_control_plane::{serve, serve_ha, serve_witness, AppState};
use anyhow::Result;

const RUNTIME_FILE_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn instance_lock_path(db_path: &str) -> PathBuf {
    PathBuf::from(format!("{db_path}.lock"))
}

/// Guard against two control-plane processes opening the same SQLite DB on one
/// host. SQLite is single-writer; a second instance would spam "database is
/// locked" or risk corruption. We hold an exclusive flock on `<db>.lock` for
/// the process lifetime. flock is released automatically on exit (incl. crash
/// / SIGKILL), so there are no stale pid files to clean up. Non-file DBs
/// (":memory:", used by tests) are skipped.
#[cfg(unix)]
fn acquire_instance_lock(db_path: &str) -> anyhow::Result<Option<std::fs::File>> {
    if db_path.is_empty() || db_path == ":memory:" {
        return Ok(None);
    }
    let lock_path = instance_lock_path(db_path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)?;
    // SAFETY: `file` owns the fd for its lifetime; we never close it early.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            anyhow::bail!(
                "control plane already running: lock held on {} (another instance on this host?)",
                lock_path.display()
            );
        }
        return Err(e.into());
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
fn acquire_instance_lock(_db_path: &str) -> anyhow::Result<Option<std::fs::File>> {
    Ok(None)
}

/// Runtime files must outlive the process. If an operator or temp cleaner
/// removes the database directory, SQLite reconnects fail every few seconds
/// and the process keeps appending warnings to its now-deleted stdout log.
/// Exit instead: dropping the process releases the instance flock and closes
/// the log descriptor.
#[cfg(unix)]
fn runtime_files_intact(db_path: &Path, lock_file: Option<&std::fs::File>) -> bool {
    use std::os::unix::fs::MetadataExt;

    if !db_path.exists() {
        return false;
    }
    let Some(lock_file) = lock_file else {
        return true;
    };
    let Ok(locked) = lock_file.metadata() else {
        return false;
    };
    let Ok(path) = std::fs::metadata(instance_lock_path(&db_path.to_string_lossy())) else {
        return false;
    };
    locked.dev() == path.dev() && locked.ino() == path.ino()
}

#[cfg(not(unix))]
fn runtime_files_intact(db_path: &Path, _lock_file: Option<&std::fs::File>) -> bool {
    db_path.exists()
}

async fn wait_for_runtime_failure(
    db_path: &Path,
    lock_file: Option<&std::fs::File>,
    poll_interval: Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(poll_interval).await;
        if !runtime_files_intact(db_path, lock_file) {
            anyhow::bail!(
                "control-plane runtime files disappeared: {}",
                db_path.display()
            );
        }
    }
}

async fn watch_runtime_files(db_path: &str, lock_file: Option<&std::fs::File>) -> Result<()> {
    if db_path.is_empty() || db_path == ":memory:" {
        return std::future::pending::<Result<()>>().await;
    }
    wait_for_runtime_failure(Path::new(db_path), lock_file, RUNTIME_FILE_POLL_INTERVAL).await
}

// Plan 6.4 process topology: the control plane is I/O-bound (axum + sqlite
// + scheduler ticks) — a small worker pool keeps idle RSS within the 64 MiB
// budget; blocking DB/IO work goes through spawn_blocking on a bounded pool.
// Env overrides for operators: AGENTGRID_TOKIO_WORKERS,
// AGENTGRID_TOKIO_BLOCKING.
fn tokio_runtime() -> tokio::runtime::Runtime {
    let workers = std::env::var("AGENTGRID_TOKIO_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let blocking = std::env::var("AGENTGRID_TOKIO_BLOCKING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn main() -> Result<()> {
    // OpenTelemetry metrics (optional feature)
    #[cfg(feature = "opentelemetry")]
    let trace_provider = {
        agentgrid_control_plane::otel::init_otel()?;
        agentgrid_control_plane::otel::init_traces()?
    };

    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    // A `Registry` rather than `fmt().init()`: an OpenTelemetry layer has to be
    // composed with the formatter, and `fmt().init()` installs a subscriber of
    // its own that nothing can be added to (ADR 0025).
    #[cfg(feature = "opentelemetry")]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let trace_layer = trace_provider
            .as_ref()
            .map(agentgrid_control_plane::otel::trace_layer);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .with(trace_layer)
            .init();
    }
    #[cfg(not(feature = "opentelemetry"))]
    {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    }

    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("agentgrid-control-plane {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let result = tokio_runtime().block_on(amain());

    // Flush the batch processor: dropping the provider discards whatever spans
    // are still queued, which would make export look like it works most of the
    // time.
    #[cfg(feature = "opentelemetry")]
    if let Some(provider) = trace_provider {
        if let Err(e) = provider.shutdown() {
            eprintln!("otel trace shutdown failed: {e}");
        }
    }

    result
}

async fn amain() -> Result<()> {
    let addr: SocketAddr = std::env::var("AGENTGRID_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:7800".into())
        .parse()?;
    let db_path = std::env::var("AGENTGRID_DB").unwrap_or_else(|_| "control-plane.db".into());
    // Documented since 0.4 — both Dockerfiles, both install scripts, every HA
    // compose file, the README — and read nowhere until now: the artifact root
    // was always derived from the database directory, so a dedicated artifact
    // volume was silently ignored (and the disk watermark watched the wrong
    // one). Empty counts as unset: a blank line in a unit file must not create
    // a directory named "".
    let artifact_root = std::env::var("AGENTGRID_ARTIFACT_ROOT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);

    let ha = agentgrid_control_plane::ha::HaRuntime::from_env()?;
    if ha.is_ha() && ha.config.role == NodeRole::Witness {
        // A witness keeps no database and holds no instance lock; its durable
        // vote lives in AGENTGRID_HA_WITNESS_DIR.
        return serve_witness(ha, addr).await;
    }

    // Hold the single-instance lock for the whole process lifetime (the File
    // drops when `main` returns, releasing the flock). In HA each host has its
    // own database copy, so the lock still guards against two local writers.
    let instance_lock = acquire_instance_lock(&db_path)?;

    let state = AppState::open_with_runtime_and_root(&db_path, ha, artifact_root).await?;
    let server = async {
        if state.ha.is_ha() {
            serve_ha(state, addr).await
        } else {
            serve(state, addr).await
        }
    };
    tokio::select! {
        result = server => result,
        result = watch_runtime_files(&db_path, instance_lock.as_ref()) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_watcher_fails_when_database_disappears() {
        let dir = std::env::temp_dir().join(format!("ag-runtime-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("cp.db");
        let lock_path = dir.join("cp.db.lock");
        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(&lock_path, b"").unwrap();

        std::fs::remove_file(&db_path).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_runtime_failure(&db_path, None, Duration::from_millis(10)),
        )
        .await
        .expect("watcher timed out");
        assert!(result.is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_watcher_detects_replaced_lock_file() {
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!("ag-runtime-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("cp.db");
        let lock_path = dir.join("cp.db.lock");
        std::fs::write(&db_path, b"db").unwrap();
        let lock_file = std::fs::File::create(&lock_path).unwrap();

        std::fs::remove_file(&lock_path).unwrap();
        std::fs::write(&lock_path, b"replacement").unwrap();
        let locked = lock_file.metadata().unwrap();
        let replacement = std::fs::metadata(&lock_path).unwrap();
        assert!(locked.dev() != replacement.dev() || locked.ino() != replacement.ino());
        assert!(!runtime_files_intact(&db_path, Some(&lock_file)));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
