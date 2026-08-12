//! Durable execution.
//!
//! Shaped after the pattern that needs no orchestration server: a status row per run, a
//! checkpoint row per completed step, and a scan for unfinished work at startup. Two rules make
//! it work, and both are constraints on the caller rather than features of this module:
//!
//! - **The workflow is deterministic.** Given the same inputs and the same step results, it
//!   invokes the same steps with the same inputs in the same order. Recovery replays the
//!   workflow function and matches its calls against the checkpoints; a workflow that decides
//!   differently on the second pass finds a checkpoint for a step it did not expect.
//! - **A step that has been checkpointed is never re-executed.** This is the difference between
//!   this and the graph frameworks whose "replay" re-runs the nodes: their replay calls the
//!   model again and can produce a different answer, which is exploration, not reproduction.
//!   Reproducing a run that went wrong needs the recorded results, not fresh ones.
//!
//! Steps must also be idempotent, because a crash between doing the work and writing the
//! checkpoint is always possible and is indistinguishable from a crash just before the work.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use wkbd_store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Planning,
    Running,
    Blocked,
    /// Finished successfully and waiting for a human to merge.
    ///
    /// Not `Blocked`, which it superficially resembles. A blocked run has a dependency that failed
    /// and there is nothing anyone can do; this one succeeded and is waiting on a decision. Recovery
    /// treats them oppositely — one is over, the other must not be restarted — and a status that
    /// cannot tell them apart will either re-run finished work or throw away a candidate somebody
    /// was about to accept.
    AwaitingMerge,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Planning => "planning",
            RunStatus::Running => "running",
            RunStatus::Blocked => "blocked",
            RunStatus::AwaitingMerge => "awaiting_merge",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "planning" => RunStatus::Planning,
            "blocked" => RunStatus::Blocked,
            "awaiting_merge" => RunStatus::AwaitingMerge,
            "done" => RunStatus::Done,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Running,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, RunStatus::Done | RunStatus::Failed | RunStatus::Cancelled)
    }
}

/// How a step should be retried.
///
/// Declared per step rather than written into the step body, so that "how many times did we
/// try this" is data the recovery scan can read rather than control flow it would have to
/// re-enter.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub interval: std::time::Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, interval: std::time::Duration::from_secs(2) }
    }
}

pub struct Workflow {
    store: Store,
    run_id: String,
}

impl Workflow {
    pub async fn start(store: &Store, goal: &str, project_root: &str) -> Result<Self> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let id = run_id.clone();
        let goal = goal.to_string();
        let root = project_root.to_string();
        store
            .write(move |tx| {
                let now = wkbd_store::now_ms();
                tx.execute(
                    "INSERT INTO runs (id, goal, project_root, status, created_ms, updated_ms)
                     VALUES (?1, ?2, ?3, 'planning', ?4, ?4)",
                    rusqlite::params![id, goal, root, now],
                )?;
                Ok(())
            })
            .await?;
        Ok(Self { store: store.clone(), run_id })
    }

    pub fn resume(store: &Store, run_id: &str) -> Self {
        Self { store: store.clone(), run_id: run_id.to_string() }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub async fn set_status(&self, status: RunStatus) -> Result<()> {
        let id = self.run_id.clone();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE runs SET status = ?2, updated_ms = ?3 WHERE id = ?1",
                    rusqlite::params![id, status.as_str(), wkbd_store::now_ms()],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn status(&self) -> Result<RunStatus> {
        let id = self.run_id.clone();
        let s: String = self
            .store
            .read(move |conn| {
                Ok(conn.query_row("SELECT status FROM runs WHERE id = ?1", [&id], |r| r.get(0))?)
            })
            .await?;
        Ok(RunStatus::from_str(&s))
    }

    /// Runs a step once, ever.
    ///
    /// If a checkpoint exists the recorded output is returned and the closure is not called.
    /// That is the whole mechanism: after a crash, recovery walks the same path and gets the
    /// same answers for everything that already finished.
    pub async fn step<T, F, Fut>(&self, key: &str, f: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Some(existing) = self.checkpoint(key).await? {
            let value: T = serde_json::from_str(&existing)
                .with_context(|| format!("decoding checkpoint for step {key}"))?;
            tracing::debug!(run = %self.run_id, step = key, "step already checkpointed; not re-running");
            return Ok(value);
        }

        let value = f().await?;
        let encoded = serde_json::to_string(&value)?;
        self.write_checkpoint(key, encoded).await?;
        Ok(value)
    }

    /// As [`Self::step`], with retries.
    ///
    /// Attempt counts are not checkpointed: a step that failed left no checkpoint, so after a
    /// crash it starts again from attempt zero. That is the honest behaviour, since we cannot
    /// know whether the failed attempt had partial effects.
    pub async fn step_with_retry<T, F, Fut>(
        &self,
        key: &str,
        policy: RetryPolicy,
        mut f: F,
    ) -> Result<T>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Some(existing) = self.checkpoint(key).await? {
            return Ok(serde_json::from_str(&existing)?);
        }

        let mut last_error = None;
        for attempt in 0..policy.max_attempts.max(1) {
            match f(attempt).await {
                Ok(value) => {
                    let encoded = serde_json::to_string(&value)?;
                    self.write_checkpoint(key, encoded).await?;
                    return Ok(value);
                }
                Err(e) => {
                    tracing::warn!(run = %self.run_id, step = key, attempt, error = %e, "step failed");
                    last_error = Some(e);
                    if attempt + 1 < policy.max_attempts {
                        tokio::time::sleep(policy.interval).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("step {key} failed with no error")))
    }

    pub async fn checkpoint(&self, key: &str) -> Result<Option<String>> {
        let run = self.run_id.clone();
        let key = key.to_string();
        self.store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT output FROM step_outputs WHERE run_id = ?1 AND step_key = ?2",
                )?;
                let mut rows = stmt.query(rusqlite::params![run, key])?;
                match rows.next()? {
                    Some(row) => Ok(Some(row.get::<_, String>(0)?)),
                    None => Ok(None),
                }
            })
            .await
    }

    async fn write_checkpoint(&self, key: &str, output: String) -> Result<()> {
        let run = self.run_id.clone();
        let key = key.to_string();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT OR REPLACE INTO step_outputs (run_id, step_key, output, created_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![run, key, output, wkbd_store::now_ms()],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn completed_steps(&self) -> Result<Vec<String>> {
        let run = self.run_id.clone();
        self.store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT step_key FROM step_outputs WHERE run_id = ?1 ORDER BY created_ms",
                )?;
                let rows = stmt.query_map([&run], |r| r.get::<_, String>(0))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
    }
}

/// Runs that were interrupted.
///
/// Called at startup. Anything not in a terminal state was in flight when the process stopped,
/// which for a desktop application is the common case rather than an exception.
pub async fn unfinished_runs(store: &Store) -> Result<Vec<String>> {
    store
        .read(|conn| {
            // `awaiting_merge` is excluded even though it is not a terminal status. That run is
            // not waiting on us, it is waiting on a person, and re-driving it would walk every
            // checkpoint again and append a second copy of the "here is your candidate" event —
            // so the reader would see two candidates and have to work out that they are the same
            // one. The distinction only exists because the status does; folding it into `blocked`
            // would make this query unable to tell "nothing more can happen" from "somebody has
            // a decision to make".
            let mut stmt = conn.prepare(
                "SELECT id FROM runs
                 WHERE status NOT IN ('done', 'failed', 'cancelled', 'awaiting_merge')
                 ORDER BY created_ms",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}
