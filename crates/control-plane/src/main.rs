use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use agentgrid_control_plane::ha::NodeRole;
use agentgrid_control_plane::{serve, serve_ha, serve_witness, AppState};
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

    let ha = agentgrid_control_plane::ha::HaRuntime::from_env()?;
    if ha.is_ha() && ha.config.role == NodeRole::Witness {
        // A witness keeps no database and holds no instance lock; its durable
        // vote lives in AGENTGRID_HA_WITNESS_DIR.
        return serve_witness(ha, addr).await;
    }

    // Hold the single-instance lock for the whole process lifetime (the File
    // drops when `main` returns, releasing the flock). In HA each host has its
    // own database copy, so the lock still guards against two local writers.
    let _instance_lock = acquire_instance_lock(&db_path)?;

    let state = AppState::open_with_runtime(&db_path, ha).await?;
    if state.ha.is_ha() {
        serve_ha(state, addr).await
    } else {
        serve(state, addr).await
    }
}
