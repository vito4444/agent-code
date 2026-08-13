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
mod fs_bridge;
mod learn;
mod planner;
mod route;
mod run;
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

    /// Which agents may do orchestrated work. Repeatable; the first is the fallback.
    ///
    /// Nominated rather than inferred. Treating every configured agent as a candidate is what the
    /// first version did, and it sent tasks to agents set up for interactive chat — the run failed,
    /// and the reason was only visible in the sidebar, where two workers were named after agents
    /// nobody had offered for the job. An agent being present is not an agent volunteering.
    ///
    /// Naming more than one turns routing on: the bandit picks between them per task and learns from
    /// the acceptance result. Naming one keeps the router off entirely, since a bandit over one arm
    /// is arithmetic with a fixed answer.
    #[arg(long = "worker-agent")]
    worker_agents: Vec<String>,

    /// Which agent drafts task graphs. Defaults to the worker agent.
    #[arg(long)]
    planner_agent: Option<String>,

    /// Relative price of an agent, as `id=number`. Repeatable.
    ///
    /// Declared, never measured: the daemon is told a command line and nothing about what running it
    /// costs. With none of these the router's price penalty multiplies by a constant and the
    /// objective degenerates to "the arm most likely to pass" — which is a real limit rather than a
    /// bug, since inventing prices would make it confidently optimise something nobody measured.
    #[arg(long = "agent-cost", value_parser = parse_agent_cost)]
    agent_costs: Vec<(String, f64)>,

    /// Read task graphs from this JSON file instead of asking a model.
    ///
    /// For tests and for reproducing a run: a graph is a graph, and everything downstream behaves
    /// identically whether a model or a file produced it. That is what lets the end-to-end test
    /// exercise a real multi-task run against a real repository with no credentials.
    #[arg(long)]
    fixed_plan: Option<std::path::PathBuf>,

    /// Do not offer `fs/read_text_file` and `fs/write_text_file` to agents.
    ///
    /// Not the safe option, despite looking like one. An agent that cannot ask us to read a file
    /// reads it itself, in its own process, and we see nothing and enforce nothing. Offering it
    /// puts every access behind a kernel-decided boundary and into the event log.
    #[arg(long)]
    no_client_fs: bool,
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
    // The registry is what makes the boot-side sweep above mean anything: it reads this file, so
    // something has to write it. Without this the third cleanup layer is present in the code and
    // absent in effect.
    let pool = Arc::new(AgentPool::new(incoming_tx, Some(raw_tx)).with_registry(registry.clone()));
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
        offer_client_fs: !cli.no_client_fs,
        runs: std::sync::Mutex::new(None),
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

    // Built after the state because it holds a reference to it: a run opens agent sessions, and
    // those live in the state.
    if let Some(worker) = cli.worker_agents.first() {
        let planner: Arc<dyn planner::Planner> = match &cli.fixed_plan {
            Some(path) => {
                let body = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                // One graph, or a list of them. The list form is how a test scripts "the first
                // graph is rejected, the second is accepted".
                let graphs: Vec<wkbd_orch::DraftGraph> =
                    match serde_json::from_str::<Vec<wkbd_orch::DraftGraph>>(&body) {
                        Ok(list) => list,
                        Err(_) => vec![serde_json::from_str(&body)
                            .with_context(|| format!("parsing {}", path.display()))?],
                    };
                Arc::new(planner::FixedPlanner::new(graphs))
            }
            None => Arc::new(planner::AgentPlanner {
                state: app_state.clone(),
                agent_id: cli.planner_agent.clone().unwrap_or_else(|| worker.clone()),
                project_root: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".to_string()),
            }),
        };

        // The nominated agents, and only those. Routing turns itself off when one is named, so the
        // common setup pays nothing for it.
        //
        // An agent that was configured for interactive chat has not offered to take orchestrated
        // work, and giving it some produces a run that fails for a reason visible only in the
        // sidebar — where the worker rows are named after agents nobody nominated.
        let unknown: Vec<&String> = cli
            .worker_agents
            .iter()
            .filter(|id| !app_state.agents.iter().any(|a| &&a.id == id))
            .collect();
        if !unknown.is_empty() {
            // Refused at startup rather than at the first run. A misspelled id would otherwise
            // surface as a run that dies preparing its first worktree.
            anyhow::bail!("--worker-agent names agents that were not configured: {unknown:?}");
        }

        let costs: std::collections::BTreeMap<String, f64> =
            cli.agent_costs.iter().cloned().collect();
        let routing = match route::Routing::new(store.clone(), &cli.worker_agents, &costs).await {
            Ok(r) => Some(r),
            Err(e) => {
                // Routing is an optimisation. A router that will not start must not stop runs from
                // happening; the configured worker does the work, as it would with no router at all.
                tracing::error!(error = %e, "could not start the router; using the configured worker");
                None
            }
        };

        let engine = Arc::new(run::RunEngine {
            store: store.clone(),
            state: app_state.clone(),
            planner,
            worker_agent: worker.clone(),
            // Outside the repository. A worktree inside it appears in the repository's own status
            // and in its globs, and then one task's ownership check starts seeing another task's
            // files.
            worktree_root: state_dir.join("worktrees"),
            cancelled: std::sync::Mutex::new(Default::default()),
            routing,
        });
        app_state.set_runs(engine.clone());

        // Runs a crash interrupted. Every finished step returns its checkpoint instead of running
        // again, so this resumes rather than restarts: re-creating an existing worktree fails, and
        // re-dispatching a finished task pays for the work twice.
        if !cli.safe_mode {
            match wkbd_orch::unfinished_runs(&store).await {
                Ok(ids) => {
                    for id in ids {
                        tracing::info!(run = %id, "resuming an unfinished run");
                        if let Err(e) = engine.resume(&id).await {
                            // Nothing on the startup path may make the application unopenable, so a
                            // run that will not resume is logged and skipped.
                            tracing::error!(run = %id, error = %e, "could not resume");
                        }
                    }
                }
                Err(e) => tracing::error!(error = %e, "could not scan for unfinished runs"),
            }
        }
    }

    let mut app = api::router(app_state.clone());

    // Found rather than required. `--ui-dir` used to be the only way to serve the interface, which
    // meant a caller that did not know to pass it got a daemon answering 404 on every path except
    // `/api/*` — and a desktop shell pointed at it showed a white window with nothing anywhere
    // saying why. That is what happened the first time this was tried, because the shell asserted in
    // a comment that the daemon served the interface, and nothing had ever checked.
    match cli.ui_dir.clone().or_else(api::find_interface) {
        Some(dist) if dist.join("index.html").is_file() => {
            tracing::info!(path = %dist.display(), "serving the interface");
            app = api::with_interface(app, &dist);
        }
        Some(dist) => tracing::warn!(
            path = %dist.display(),
            "that directory has no index.html; serving the API only"
        ),
        None => tracing::warn!(
            "no built interface found; the API is up but there is nothing to open in a browser. \
             Build it with `cd ui && pnpm build`."
        ),
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

/// `id=1.5`.
fn parse_agent_cost(raw: &str) -> Result<(String, f64), String> {
    let (id, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected id=number, got {raw:?}"))?;
    let cost: f64 = value
        .parse()
        .map_err(|_| format!("{value:?} is not a number"))?;
    if !cost.is_finite() || cost <= 0.0 {
        // A zero or negative price makes the penalty term meaningless rather than generous, and a
        // NaN poisons every later comparison silently.
        return Err(format!("a price must be a positive number, got {cost}"));
    }
    Ok((id.to_string(), cost))
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
