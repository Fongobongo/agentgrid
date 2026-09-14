use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use agentgrid_control_plane::{serve, AppState};
use anyhow::Result;

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
    let lock_path = format!("{db_path}.lock");
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
                "control plane already running: lock held on {lock_path} (another instance on this host?)"
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
    {
        agentgrid_control_plane::otel::init_otel()?;
    }

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("agentgrid-control-plane {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tokio_runtime().block_on(amain())
}

async fn amain() -> Result<()> {
    let addr: SocketAddr = std::env::var("AGENTGRID_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:7800".into())
        .parse()?;
    let db_path = std::env::var("AGENTGRID_DB").unwrap_or_else(|_| "control-plane.db".into());

    // Hold the single-instance lock for the whole process lifetime (the File
    // drops when `main` returns, releasing the flock).
    let _instance_lock = acquire_instance_lock(&db_path)?;

    let state = AppState::open(&db_path).await?;
    serve(state, addr).await
}
