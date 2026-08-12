//! Daemon state and the message dispatcher.
//!
//! One agent process can host several sessions, so the pool's single inbound channel has to
//! be demultiplexed by the session id inside each message. Getting that wrong sends one
//! session's tool output to another's transcript, which is the kind of bug that looks like
//! the model behaving strangely.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use wkbd_agent::{
    AgentPool, AgentSpec, Direction, Incoming, LaunchConfig, ProcessKey, RawFrame, SessionFactory,
    SessionHandle, SessionOpenRequest, SessionPurpose,
};
use wkbd_proto::Event;
use wkbd_sec::permission::PermissionStore;
use wkbd_store::Store;

/// A frame as the inspector shows it.
#[derive(Clone, serde::Serialize)]
pub struct InspectorFrame {
    pub at_ms: i64,
    pub direction: &'static str,
    pub agent_id: String,
    pub line: String,
    pub malformed: bool,
}

pub struct LiveSession {
    pub handle: Arc<SessionHandle>,
    pub agent_id: String,
    pub agent_display_name: String,
    pub project_root: String,
    pub title: Option<String>,
    /// Inbound messages for this session only.
    pub inbox: Arc<Mutex<mpsc::UnboundedReceiver<Incoming>>>,
    pub inbox_tx: mpsc::UnboundedSender<Incoming>,
    /// Set while a turn is running, so a second prompt cannot start one concurrently.
    pub busy: Arc<Mutex<bool>>,
}

pub struct AppState {
    pub store: Store,
    pub pool: Arc<AgentPool>,
    pub factory: SessionFactory,
    pub permissions: Arc<Mutex<PermissionStore>>,
    pub agents: Vec<AgentSpec>,
    pub sessions: RwLock<HashMap<String, Arc<LiveSession>>>,
    /// Maps the agent's own session id to our local id, for demultiplexing.
    pub acp_to_local: RwLock<HashMap<String, String>>,
    pub events: broadcast::Sender<Event>,
    pub raw: Mutex<Vec<InspectorFrame>>,
    pub degraded: Option<String>,
    pub state_dir: std::path::PathBuf,
    /// Permission requests currently on screen, keyed by request id.
    pub pending_permissions: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Option<String>>>>,
}

impl AppState {
    pub fn agent(&self, id: &str) -> Option<&AgentSpec> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub async fn session(&self, local_id: &str) -> Option<Arc<LiveSession>> {
        self.sessions.read().await.get(local_id).cloned()
    }

    pub async fn open_session(
        self: &Arc<Self>,
        agent_id: &str,
        project_root: &str,
        purpose: SessionPurpose,
    ) -> Result<Arc<LiveSession>> {
        let spec = self.agent(agent_id).context("no such agent")?.clone();
        let config = LaunchConfig {
            values: Default::default(),
            cwd: project_root.to_string(),
        };

        let handle = self
            .factory
            .open(
                SessionOpenRequest {
                    spec: spec.clone(),
                    config,
                    project_root: project_root.to_string(),
                    purpose,
                    resume_acp_session_id: None,
                    handoff_summary: None,
                },
                |spec, config| build_agent_command(spec, config),
            )
            .await?;

        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let session = Arc::new(LiveSession {
            agent_id: spec.id.clone(),
            agent_display_name: spec.display_name.clone(),
            project_root: project_root.to_string(),
            title: None,
            inbox: Arc::new(Mutex::new(inbox_rx)),
            inbox_tx,
            busy: Arc::new(Mutex::new(false)),
            handle: Arc::new(handle),
        });

        let local_id = session.handle.local_id.clone();
        self.acp_to_local
            .write()
            .await
            .insert(session.handle.acp_session_id.clone(), local_id.clone());
        self.sessions.write().await.insert(local_id.clone(), session.clone());

        // The options the agent declared at `session/new` have to reach the client as an event.
        //
        // They are the model and thinking-level selectors. Holding them only in memory means the
        // interface sees an empty list and correctly draws nothing — so the feature looks absent
        // rather than broken, which makes it exactly the kind of gap that survives review. The
        // event also puts them in the log, so a reconnecting client and a replayed run see the
        // same controls as the original.
        if !session.handle.config_options.is_empty() {
            let payload = wkbd_proto::EventPayload::ConfigOptionsChanged {
                options: session.handle.config_options.clone(),
            };
            match self.store.append_one(local_id.clone(), payload).await {
                Ok(event) => self.publish(&[event]),
                Err(e) => tracing::warn!(error = %e, "could not record the agent's config options"),
            }
        }

        // Recording the session is best effort. Failing to persist the row must not stop a
        // session that is already running: the event log is the source of truth and this
        // table is a convenience index over it.
        let persist = self
            .store
            .write({
                let local_id = local_id.clone();
                let agent_id = spec.id.clone();
                let project_root = project_root.to_string();
                let acp_id = session.handle.acp_session_id.clone();
                let fingerprint = session.handle.process_key.fingerprint.clone();
                move |tx| {
                    tx.execute(
                        "INSERT OR REPLACE INTO sessions
                         (id, agent_id, project_root, created_ms, acp_session_id, config_fingerprint)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            local_id,
                            agent_id,
                            project_root,
                            wkbd_store::now_ms(),
                            acp_id,
                            fingerprint
                        ],
                    )?;
                    Ok(())
                }
            })
            .await;
        if let Err(e) = persist {
            tracing::warn!(error = %e, "could not record the session row");
        }

        Ok(session)
    }

    /// Records that a turn is waiting on a human for `request_id`.
    pub async fn register_permission_wait(
        &self,
        request_id: &str,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_permissions
            .lock()
            .await
            .insert(request_id.to_string(), tx);
        rx
    }

    pub async fn clear_permission_wait(&self, request_id: &str) {
        self.pending_permissions.lock().await.remove(request_id);
    }

    /// Delivers an answer. Returns false when nothing was waiting, which happens when the
    /// turn was cancelled while the prompt was still on screen.
    pub async fn deliver_permission_answer(
        &self,
        request_id: &str,
        option_id: Option<String>,
    ) -> bool {
        let waiter = self.pending_permissions.lock().await.remove(request_id);
        match waiter {
            Some(tx) => tx.send(option_id).is_ok(),
            None => false,
        }
    }

    /// Publishes events to every connected client.
    ///
    /// Subscribers that fall behind are told to resume from the log by sequence number rather
    /// than being sent a best guess, which is why lagging is survivable.
    pub fn publish(&self, events: &[Event]) {
        for event in events {
            let _ = self.events.send(event.clone());
        }
    }

    /// Routes one inbound message to the session it belongs to.
    async fn route(&self, key: ProcessKey, msg: Incoming) {
        let acp_session_id = match &msg {
            Incoming::Notification { params, .. } | Incoming::Request { params, .. } => params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };

        let local = match acp_session_id {
            Some(acp) => self.acp_to_local.read().await.get(&acp).cloned(),
            None => None,
        };

        match local {
            Some(local) => {
                if let Some(session) = self.sessions.read().await.get(&local) {
                    let _ = session.inbox_tx.send(msg);
                }
            }
            None => {
                // A message we cannot attribute is logged rather than guessed at. Delivering
                // it to an arbitrary session would put one conversation's tool output in
                // another's transcript.
                tracing::warn!(
                    agent = %key.agent_id,
                    "inbound message with no matching session; dropping"
                );
            }
        }
    }
}

/// Builds the command line for one agent.
///
/// The environment is constructed, not inherited. Whoever controls a child's environment
/// controls configuration that git treats as more trusted than anything in the repository,
/// so inheriting and deleting a few names is the wrong shape: the list of names to delete is
/// open-ended and grows with every tool the agent might invoke.
pub fn build_agent_command(spec: &AgentSpec, config: &LaunchConfig) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args);
    cmd.env_clear();

    for name in ["PATH", "LANG", "LC_ALL", "TZ", "TERM"] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Launch-time settings the agent cannot change later. The template says how each one is
    // delivered, because agents disagree: some read argv, some read the environment.
    for (option_id, value) in &config.values {
        if let Some(template) = spec.launch_config.get(option_id) {
            if let Some(rest) = template.strip_prefix("env:") {
                if let Some((name, pattern)) = rest.split_once('=') {
                    cmd.env(name, pattern.replace("{}", value));
                }
            } else {
                cmd.arg(template.replace("{}", value));
            }
        }
    }

    cmd.current_dir(&config.cwd);
    // Last resort only: the supervisor owns process lifetime. This covers the case where the
    // daemon panics and unwinds without reaching the supervisor's cleanup.
    cmd.kill_on_drop(true);
    cmd
}

/// Starts the dispatcher and the raw-frame recorder.
pub fn spawn_dispatchers(
    state: Arc<AppState>,
    mut incoming: mpsc::UnboundedReceiver<(ProcessKey, Incoming)>,
    mut raw: mpsc::UnboundedReceiver<(ProcessKey, RawFrame)>,
) {
    {
        let state = state.clone();
        tokio::spawn(async move {
            while let Some((key, msg)) = incoming.recv().await {
                state.route(key, msg).await;
            }
            tracing::info!("agent message dispatcher stopped");
        });
    }

    tokio::spawn(async move {
        while let Some((key, frame)) = raw.recv().await {
            let mut log = state.raw.lock().await;
            // Bounded ring. The inspector is a debugging aid, not an archive, and an
            // unbounded buffer of every frame from a long session is a memory leak with a
            // user interface.
            if log.len() >= 5_000 {
                log.drain(0..1_000);
            }
            log.push(InspectorFrame {
                at_ms: frame.at_ms,
                direction: match frame.direction {
                    Direction::ToAgent => "to_agent",
                    Direction::FromAgent => "from_agent",
                },
                agent_id: key.agent_id.clone(),
                line: frame.line,
                malformed: frame.malformed,
            });
        }
    });
}
