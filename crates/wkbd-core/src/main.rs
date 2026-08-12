//! The workbench daemon.
//!
//! Owns every agent subprocess, the event log and the HTTP surface. The desktop shell is a
//! client of this process, not its host, which is what makes "the window crashed" and "the work
//! stopped" separate events. It is also why remote access and headless use need no second
//! implementation.
//!
//! The startup path is deliberately paranoid. It restores previous sessions, previous worktrees
//! and a previous event stream — all data a previous run wrote, and any of which a bug in a
//! previous run may have corrupted. If restoring that is what crashes, every subsequent launch
//! crashes identically and the user has no way in. So each stage is allowed to fail without
//! taking the process with it, and a launch that never reaches a serving state raises a counter
//! that eventually stops the restore from being attempted at all.

mod api;
mod runner;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use wkbd_agent::{AgentPool, AgentSpec, SessionFactory};
use wkbd_sec::permission::PermissionStore;
use wkbd_sec::reaper::Supervisor;
use wkbd_store::{BootGuard, Store};

use state::AppState;

#[derive(Parser, Debug)]
#[command(name = "wkbd-core", about = "Multi-agent workbench daemon")]
struct Cli {
    /// Where the database, blobs and boot state live.
    #[arg(long, env = "WKBD_STATE_DIR")]
    state_dir: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:8787", env = "WKBD_LISTEN")]
    listen: String,

    /// Directory of built interface assets. Omitted means API only, which is what the interface
    /// dev server expects.
    #[arg(long, env = "WKBD_UI_DIR")]
    ui_dir: Option<PathBuf>,

    /// Agent definitions, repeatable: `--agent 'id=Display Name=command arg arg'`.
    #[arg(long = "agent")]
    agents: Vec<String>,

    /// Start without restoring anything from the previous run.
    #[arg(long)]
    safe_mode: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wkbd=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let state_dir = cli
        .state_dir
        .clone()
        .or_else(|| dirs_state_dir())
        .context("could not determine a state directory")?;

    // Raised before any restore work happens and cleared only once we are serving. A process
    // that dies in between leaves it raised, which is the signal we want: success has to be
    // affirmed rather than assumed.
    let (mut boot, outcome) = BootGuard::begin(&state_dir)?;
    if outcome.previous_launch_failed {
        tracing::warn!(
            failures = outcome.consecutive_failures,
            mode = ?outcome.safe_mode,
            "the previous launch did not reach a serving state"
        );
    }
    let safe_mode = if cli.safe_mode {
        wkbd_store::SafeMode::Minimal
    } else {
        outcome.safe_mode
    };

    // Anything left over from a previous run is swept before we start our own children. The
    // sweep matches on process id *and* executable name, because process ids are reused and
    // matching on the id alone eventually kills something unrelated.
    let registry = state_dir.join("processes.json");
    match Supervisor::sweep_orphans(&registry) {
        Ok(report) => {
            if !report.killed.is_empty() || !report.identity_mismatch.is_empty() {
                tracing::info!(
                    killed = report.killed.len(),
                    // A recorded id whose executable no longer matches: the id was reused by
                    // something unrelated, so it is left alone. Matching on the id alone would
                    // eventually kill a stranger's process.
                    identity_mismatch = report.identity_mismatch.len(),
                    already_gone = report.already_gone.len(),
                    "swept processes left by a previous run"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "orphan sweep failed; continuing"),
    }

    // Orphaned grandchildren reparent here rather than to init, so they remain reachable and can
    // still be killed. Without this, an agent that forks and exits leaves its children beyond
    // our reach entirely.
    if let Err(e) = wkbd_sec::reaper::install_child_subreaper() {
        tracing::warn!(error = %e, "could not become a child subreaper; grandchildren may leak");
    }

    let opened = Store::open(&state_dir).context("opening the store")?;
    if let Some(reason) = &opened.degraded {
        // Read-only rather than refusing to start. A workbench that will not open cannot be
        // used to rescue the work inside it.
        tracing::error!(reason = %reason, "running read-only");
    }
    let store = opened.store;

    let agents = parse_agents(&cli.agents)?;
    if agents.is_empty() {
        tracing::warn!(
            "no agents configured; pass --agent 'id=Name=command args'. The interface will \
             load but cannot open a session."
        );
    }

    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(incoming_tx, Some(raw_tx)));
    let prelude = wkbd_memory::prelude_provider(store.clone());
    let factory = SessionFactory::new(pool.clone(), prelude);

    // A generous buffer, because a client that falls behind is told to resume from the log by
    // sequence number rather than being sent a guess. Lagging is survivable; losing an event
    // silently is not.
    let (events_tx, _) = broadcast::channel(4_096);

    let app_state = Arc::new(AppState {
        store: store.clone(),
        pool: pool.clone(),
        factory,
        permissions: Arc::new(Mutex::new(PermissionStore::new())),
        agents,
        sessions: RwLock::new(Default::default()),
        acp_to_local: RwLock::new(Default::default()),
        events: events_tx,
        raw: Mutex::new(Vec::new()),
        degraded: opened.degraded.clone(),
        state_dir: state_dir.clone(),
        pending_permissions: Mutex::new(Default::default()),
    });

    state::spawn_dispatchers(app_state.clone(), incoming_rx, raw_rx);

    if safe_mode.restores_sessions() {
        // Per-run isolation: one unfinished run that cannot be reported must not stop the rest
        // from being reported, and none of them may stop the daemon from serving.
        match wkbd_orch::unfinished_runs(&store).await {
            Ok(runs) if !runs.is_empty() => {
                tracing::info!(count = runs.len(), "runs were interrupted by the last shutdown");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not list unfinished runs"),
        }
    } else {
        tracing::warn!(mode = ?safe_mode, "safe mode: not restoring previous state");
    }

    let mut app = api::router(app_state.clone());
    if let Some(ui_dir) = &cli.ui_dir {
        if ui_dir.is_dir() {
            app = app.fallback_service(
                tower_http::services::ServeDir::new(ui_dir)
                    .fallback(tower_http::services::ServeFile::new(ui_dir.join("index.html"))),
            );
            tracing::info!(dir = %ui_dir.display(), "serving the interface");
        } else {
            tracing::warn!(dir = %ui_dir.display(), "interface directory does not exist");
        }
    }

    let addr: SocketAddr = cli.listen.parse().context("parsing --listen")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "daemon listening");

    // Serving is the definition of healthy. Clearing the counter earlier would mean a crash
    // during restore looked like a successful launch.
    boot.mark_healthy();

    let shutdown_state = app_state.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down; killing agent processes");
            shutdown_state.pool.shutdown().await;
        })
        .await;

    // Belt and braces: the graceful path above already does this, but a serve error skips it,
    // and leaving agent processes behind is the failure mode this whole layer exists to avoid.
    app_state.pool.shutdown().await;
    result.context("serving")
}

/// Parses `id=Display Name=command args...`.
fn parse_agents(specs: &[String]) -> Result<Vec<AgentSpec>> {
    let mut out = Vec::new();
    for raw in specs {
        let mut parts = raw.splitn(3, '=');
        let id = parts.next().unwrap_or("").trim().to_string();
        let display = parts.next().unwrap_or("").trim().to_string();
        let command = parts.next().unwrap_or("").trim().to_string();
        if id.is_empty() || command.is_empty() {
            anyhow::bail!("could not parse --agent {raw:?}; expected id=Display Name=command args");
        }
        let mut argv = command.split_whitespace().map(str::to_string);
        let program = argv.next().context("agent command is empty")?;
        out.push(AgentSpec {
            id,
            display_name: if display.is_empty() { program.clone() } else { display },
            command: program,
            args: argv.collect(),
            env: BTreeMap::new(),
            // Empty until the capability probe has observed otherwise. Assuming an agent can
            // change a setting at runtime and being wrong produces a control that appears to
            // work and changes nothing; assuming it cannot only costs an extra process.
            live_config_ids: Vec::new(),
            launch_config: BTreeMap::new(),
        });
    }
    Ok(out)
}

/// Waits for either interrupt or terminate.
///
/// Both, not just interrupt. Every process manager — a service supervisor, a container runtime,
/// a plain `kill` — sends SIGTERM, and a daemon that only listens for SIGINT skips all of its
/// cleanup in exactly the situations where cleanup matters most. The visible symptom is agent
/// processes surviving the workbench, which is the leak this layer exists to prevent.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not listen for SIGTERM");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received interrupt"),
            _ = term.recv() => tracing::info!("received terminate"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn dirs_state_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(dir).join("wkbd"));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state/wkbd"))
}
