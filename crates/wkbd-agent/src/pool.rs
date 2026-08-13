//! The agent process pool.
//!
//! The key is `(agent id, fingerprint of every launch-time setting)`. Anything that the
//! agent can only read at startup — the model in `argv` or the environment, a thinking
//! level baked into a profile, the working directory — goes into the fingerprint. Two
//! sessions whose fingerprints differ can never share a process, and the pool makes that
//! impossible to get wrong by construction rather than by convention.
//!
//! This costs nothing when an agent does support runtime reconfiguration: that agent's
//! fingerprint simply does not include the settings it can change on the fly, so all its
//! sessions land in one slot.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::conn::{Connection, Incoming, RawFrame};

/// How to launch one agent, and what it is able to reconfigure at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub id: String,
    pub display_name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Environment entries this agent needs. Everything not listed here is stripped: an
    /// inherited environment is a configuration channel we do not control.
    pub env: BTreeMap<String, String>,
    /// Config option ids the agent accepts through `session/set_config_option` while a
    /// session is running. Populated by the capability probe, empty until then, and
    /// empty is the safe default because it only costs an extra process.
    pub live_config_ids: Vec<String>,
    /// Config option ids that must instead be supplied at launch, and how. The value is
    /// a template such as `--model={}` or `env:MODEL={}`.
    pub launch_config: BTreeMap<String, String>,
}

impl AgentSpec {
    /// True when this option can be changed without restarting the process.
    pub fn is_live(&self, option_id: &str) -> bool {
        self.live_config_ids.iter().any(|i| i == option_id)
    }
}

/// The settings chosen for a particular session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchConfig {
    pub values: BTreeMap<String, String>,
    pub cwd: String,
}

impl LaunchConfig {
    /// Fingerprint of only the settings that require a restart.
    ///
    /// Options the agent can change at runtime are excluded, so switching a model on an
    /// agent that supports it does not fragment the pool.
    pub fn fingerprint(&self, spec: &AgentSpec) -> String {
        let mut h = Sha256::new();
        h.update(self.cwd.as_bytes());
        h.update([0]);
        for (k, v) in &self.values {
            if spec.is_live(k) {
                continue;
            }
            h.update(k.as_bytes());
            h.update([b'=']);
            h.update(v.as_bytes());
            h.update([0]);
        }
        hex::encode(&h.finalize()[..16])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcessKey {
    pub agent_id: String,
    pub fingerprint: String,
}

struct PooledProcess {
    conn: Arc<Connection>,
    sessions: usize,
}

/// Owns every agent subprocess in the daemon.
pub struct AgentPool {
    processes: Mutex<HashMap<ProcessKey, PooledProcess>>,
    incoming: mpsc::UnboundedSender<(ProcessKey, Incoming)>,
    raw_sink: Option<mpsc::UnboundedSender<(ProcessKey, RawFrame)>>,
    /// Where spawned processes are recorded so the next boot can find them.
    ///
    /// The third layer of cleanup, and the only one that covers a hard crash: the other two —
    /// a death signal to the child and a group kill from the parent — both need this process to
    /// still be running, or at least to be unwinding. Without a registry that something actually
    /// writes to, the boot-side sweep reads an empty file forever and the layer exists only in
    /// the code.
    registry: Option<std::path::PathBuf>,
}

/// Makes a child killable and self-terminating.
///
/// Two of the three cleanup layers are installed here, and each covers what the other misses.
///
/// The process group means one `killpg` takes the agent and anything it spawned. An agent that
/// forks — and they do, to run builds and test suites — leaves children that a kill aimed at the
/// agent alone does not touch.
///
/// The death signal means a child dies with us even if we never get to run any cleanup, which is
/// what happens on `SIGKILL` or a panic that aborts. It only reaches direct children, so it does
/// not replace the group kill; it covers the case where there is nobody left to perform one.
fn harden_lifetime(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
        let parent = std::process::id();
        unsafe {
            cmd.pre_exec(move || {
                wkbd_sec::reaper::arm_child_death_signal(parent)?;
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

impl AgentPool {
    pub fn new(
        incoming: mpsc::UnboundedSender<(ProcessKey, Incoming)>,
        raw_sink: Option<mpsc::UnboundedSender<(ProcessKey, RawFrame)>>,
    ) -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
            incoming,
            raw_sink,
            registry: None,
        }
    }

    /// Records spawned processes at this path so a later boot can sweep them.
    pub fn with_registry(mut self, path: std::path::PathBuf) -> Self {
        self.registry = Some(path);
        self
    }

    pub fn key_for(spec: &AgentSpec, config: &LaunchConfig) -> ProcessKey {
        ProcessKey { agent_id: spec.id.clone(), fingerprint: config.fingerprint(spec) }
    }

    /// Returns the connection for this key, launching a process if there is not one yet.
    pub async fn acquire(
        &self,
        spec: &AgentSpec,
        config: &LaunchConfig,
        mut build_command: impl FnMut(&AgentSpec, &LaunchConfig) -> tokio::process::Command,
    ) -> Result<Arc<Connection>> {
        let key = Self::key_for(spec, config);

        {
            let mut procs = self.processes.lock().await;
            if let Some(p) = procs.get_mut(&key) {
                p.sessions += 1;
                return Ok(p.conn.clone());
            }
        }

        let mut cmd = build_command(spec, config);
        harden_lifetime(&mut cmd);

        // Fan the per-process channels into the pool-wide ones, tagging with the key so
        // the daemon can tell which process a message came from.
        let (tx_in, mut rx_in) = mpsc::unbounded_channel::<Incoming>();
        {
            let out = self.incoming.clone();
            let key = key.clone();
            tokio::spawn(async move {
                while let Some(msg) = rx_in.recv().await {
                    if out.send((key.clone(), msg)).is_err() {
                        break;
                    }
                }
            });
        }

        let raw_tx = self.raw_sink.as_ref().map(|sink| {
            let (tx, mut rx) = mpsc::unbounded_channel::<RawFrame>();
            let out = sink.clone();
            let key = key.clone();
            tokio::spawn(async move {
                while let Some(frame) = rx.recv().await {
                    if out.send((key.clone(), frame)).is_err() {
                        break;
                    }
                }
            });
            tx
        });

        let conn = Arc::new(Connection::spawn(cmd, tx_in, raw_tx).await?);

        if let (Some(registry), Some(pid)) = (&self.registry, conn.id().await) {
            // The child is its own group leader, so the group id is the pid. Recorded after the
            // spawn rather than before, because there is nothing to record until there is a pid,
            // and a crash in the window between them leaves a process the sweep will not find —
            // which is why the sweep is the third layer and not the first.
            if let Err(e) = wkbd_sec::reaper::record_spawned(registry, pid, pid, &spec.id) {
                tracing::warn!(error = %e, pid, "could not record a spawned agent");
            }
        }

        let mut procs = self.processes.lock().await;
        // Another task may have raced us here; keep whichever landed first so we never
        // hold two processes for one key.
        if let Some(existing) = procs.get_mut(&key) {
            existing.sessions += 1;
            let winner = existing.conn.clone();
            drop(procs);
            let _ = conn.kill().await;
            return Ok(winner);
        }
        procs.insert(key, PooledProcess { conn: conn.clone(), sessions: 1 });
        Ok(conn)
    }

    /// Drops one session's claim; kills the process when the last one goes.
    pub async fn release(&self, key: &ProcessKey) -> Result<()> {
        let mut procs = self.processes.lock().await;
        let Some(p) = procs.get_mut(key) else { return Ok(()) };
        p.sessions = p.sessions.saturating_sub(1);
        if p.sessions == 0 {
            if let Some(p) = procs.remove(key) {
                drop(procs);
                self.kill_and_forget(&p.conn).await?;
            }
        }
        Ok(())
    }

    async fn kill_and_forget(&self, conn: &Arc<Connection>) -> Result<()> {
        let pid = conn.id().await;
        conn.kill().await?;
        if let (Some(registry), Some(pid)) = (&self.registry, pid) {
            // Removed only after the kill has been waited on. Forgetting first would leave a live
            // process with no record of it anywhere, which is the one state the registry exists to
            // prevent.
            if let Err(e) = wkbd_sec::reaper::forget_spawned(registry, pid) {
                tracing::debug!(error = %e, pid, "could not drop a registry entry");
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        let drained: Vec<(ProcessKey, PooledProcess)> = {
            let mut procs = self.processes.lock().await;
            procs.drain().collect()
        };
        for (key, p) in drained {
            if let Err(e) = self.kill_and_forget(&p.conn).await {
                tracing::warn!(agent = %key.agent_id, error = %e, "failed to kill agent");
            }
        }
    }

    pub async fn live_keys(&self) -> Vec<ProcessKey> {
        self.processes.lock().await.keys().cloned().collect()
    }

    pub async fn process_count(&self) -> usize {
        self.processes.lock().await.len()
    }
}
