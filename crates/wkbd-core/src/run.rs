//! Driving one run from a sentence to a merge candidate.
//!
//! Every decision here is ordinary code. The model appears twice — drafting the graph, redrafting
//! it when a deterministic predicate rejected it — and both are behind [`Planner`]. What that buys
//! is a run you can read: each step's inputs and outputs are checkpointed, so the log answers "why
//! did it do that" rather than requiring you to sample the model again and hope.
//!
//! ## Isolation is real, not requested
//!
//! Each task gets a git worktree on its own branch. Not a prompt asking the agent to stay in its
//! lane: two agents in one tree race on the index and on the files, and the failure is a corrupted
//! working tree rather than an error message. The worktree is also the boundary the path guard is
//! rooted at, so a worker cannot read another worker's files even if it tries.
//!
//! ## Dependency edges carry work
//!
//! A task with dependencies starts from a real merge of their commits, so its files already contain
//! what they produced. The alternative — putting a summary of the dependency's output in the
//! dependent's prompt — is what the well-known frameworks do, and it makes the edge carry a
//! description of the work instead of the work.
//!
//! ## Nothing merges without a human
//!
//! Verification proves the named tests pass. It does not prove the change is what was wanted, and
//! the measured rate at which models exploit a writable test suite is high enough that "the tests
//! pass" cannot be the last word. So a successful run ends at a candidate commit and stops.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use wkbd_orch::graph::AnyTest;
use wkbd_orch::{
    check_ownership, drain_merge_queue, prepare_workspace, validate, CargoTestParser, DraftGraph,
    DraftTask, MergeQueueOutcome, QueueEntry, RetryPolicy, VerifyOutcome, Verifier, Workflow,
};
use wkbd_proto::{
    run_stream_id, Event, EventPayload, PendingEvent, RunEvent, RunStatus, TaskStatus, TaskSummary,
};
use wkbd_store::Store;

use crate::planner::{describe_problems, PlanContext, Planner};
use crate::state::AppState;

/// How many times a rejected graph goes back to the planner.
///
/// Bounded because an unbounded loop against a planner that keeps making the same mistake burns
/// the user's budget without converging, and the failure it produces — "we asked eleven times" —
/// is less useful than the third rejection's problem list.
const MAX_PLAN_ATTEMPTS: u32 = 3;

/// How many times a task whose ownership check failed is redrafted.
const MAX_REPLANS: u32 = 2;

pub struct RunEngine {
    pub store: Store,
    pub state: Arc<AppState>,
    pub planner: Arc<dyn Planner>,
    /// Which agent does the planning, and which does the work. The same agent can do both; they are
    /// named separately because the useful assignment is usually not the same one.
    pub worker_agent: String,
    /// Where task worktrees are created. Outside the repository, because a worktree inside the
    /// repository shows up in the repository's own status and in globs, and then a task's
    /// `declared_paths` check starts seeing another task's files.
    pub worktree_root: PathBuf,
    /// Which agent gets each task, when there is more than one that could.
    ///
    /// `None` when routing is not configured. A bandit over one arm is arithmetic with a fixed
    /// answer, so the absence is the ordinary case rather than a degraded one.
    pub routing: Option<Arc<crate::route::Routing>>,
    /// Runs a person asked to stop.
    ///
    /// In memory rather than read back from the status column on every check. The column is the
    /// durable record — a resumed daemon must not restart a cancelled run — but a loop that
    /// re-queried it between every task would make cancellation depend on database latency, and the
    /// window that opens is exactly the one where another agent gets dispatched after the click.
    pub cancelled: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// A run's durable state, as far as this module needs it between steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRecord {
    task_id: String,
    branch: String,
    path: String,
    start_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskResult {
    task_id: String,
    /// `None` when the agent changed nothing. Reported rather than turned into an empty commit,
    /// because "decided no change was needed" and "did nothing" look identical afterwards and the
    /// difference matters.
    commit: Option<String>,
    stop_reason: String,
}

/// How a task ended, when it did not simply fail.
///
/// An ownership violation is not an error. It is an answer: the graph was wrong about what this task
/// would touch, and since declarations are what the schedule is derived from, the response is a new
/// graph rather than a failed task.
#[derive(Debug, Clone)]
enum TaskOutcome {
    /// Accepted. `None` when the agent changed nothing.
    Produced(Option<String>),
    LeftItsPaths { trigger: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifyRecord {
    passed: bool,
    missing_pass: Vec<String>,
    regressed: Vec<String>,
    detail: Option<String>,
}

impl RunEngine {
    /// Starts a run and returns its id immediately.
    ///
    /// The loop runs detached. A run outlives the request that started it by design: the HTTP call
    /// is "begin", not "do", and a client that disconnects must not cancel work that is already
    /// creating worktrees and commits.
    pub async fn start(self: &Arc<Self>, goal: &str, project_root: &str) -> Result<String> {
        let wf = Workflow::start(&self.store, goal, project_root).await?;
        let run_id = wf.run_id().to_string();

        let me = self.clone();
        let goal = goal.to_string();
        let root = project_root.to_string();
        let id = run_id.clone();
        tokio::spawn(async move {
            if let Err(e) = me.drive(wf, &goal, &root).await {
                tracing::error!(run = %id, error = %e, "run failed");
                me.emit(
                    &id,
                    RunEvent::Finished {
                        status: RunStatus::Failed,
                        detail: Some(e.to_string()),
                    },
                )
                .await;
                let _ = Workflow::resume(&me.store, &id)
                    .set_status(wkbd_orch::RunStatus::Failed)
                    .await;
            }
        });

        Ok(run_id)
    }

    /// Resumes a run that a crash interrupted.
    ///
    /// Walks the same path as the original. Every step that already finished returns its recorded
    /// output without running again, which is the entire point of the checkpoints: re-creating a
    /// worktree that exists fails, and re-dispatching a task that already ran costs the work twice
    /// and may not produce the same result.
    pub async fn resume(self: &Arc<Self>, run_id: &str) -> Result<()> {
        let wf = Workflow::resume(&self.store, run_id);
        let (goal, root) = self.run_row(run_id).await?;
        let me = self.clone();
        let id = run_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = me.drive(wf, &goal, &root).await {
                tracing::error!(run = %id, error = %e, "resumed run failed");
            }
        });
        Ok(())
    }

    pub fn is_cancelled(&self, run_id: &str) -> bool {
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(run_id)
    }

    /// Stops a run at the next point it can be stopped, and interrupts what is already running.
    ///
    /// Both halves are necessary and they do different things. Recording the cancellation stops the
    /// next task from being dispatched; cancelling the in-flight turns stops the ones already going.
    /// Doing only the first leaves agents working for a run the user has been told is over, which is
    /// worse than not offering cancellation at all — the button would report something untrue.
    ///
    /// What it does not do is undo work. Worktrees and commits that exist stay, reachable from their
    /// task branches. A cancel that deleted them would be a destructive operation behind a button
    /// labelled "stop".
    pub async fn cancel(&self, run_id: &str) -> Result<()> {
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id.to_string());

        let interrupted = self.interrupt_workers(run_id).await;

        Workflow::resume(&self.store, run_id)
            .set_status(wkbd_orch::RunStatus::Cancelled)
            .await?;
        self.emit(
            run_id,
            RunEvent::Finished {
                status: RunStatus::Cancelled,
                detail: Some(match interrupted {
                    0 => "cancelled; nothing was running".to_string(),
                    n => format!("cancelled; interrupted {n} agent turn(s) in flight"),
                }),
            },
        )
        .await;
        Ok(())
    }

    /// Cancels the turn in every session belonging to this run.
    ///
    /// Sessions are found by where they are rooted, the same way the learning pass finds them: a
    /// worker's project root is its worktree under `worktrees/<run id>/`, so no extra bookkeeping is
    /// needed to know which sessions are ours.
    async fn interrupt_workers(&self, run_id: &str) -> usize {
        let prefix = self.worktree_root.join(run_id);
        let mut count = 0;
        for session in self.state.sessions_snapshot().await {
            if !std::path::Path::new(&session.project_root).starts_with(&prefix) {
                continue;
            }
            // Best effort per session. One agent that will not answer must not prevent the others
            // from being told to stop.
            if let Err(e) = session.handle.cancel().await {
                tracing::warn!(error = %e, "could not cancel a worker turn");
            } else {
                count += 1;
            }
        }
        count
    }

    async fn drive(self: &Arc<Self>, wf: Workflow, goal: &str, project_root: &str) -> Result<()> {
        let run_id = wf.run_id().to_string();
        let repo = PathBuf::from(project_root);

        let base_commit = wf
            .step("base", || async {
                let out = wkbd_vcs::head_commit(&repo)?;
                Ok::<String, anyhow::Error>(out)
            })
            .await
            .context("resolving the base commit")?;

        self.emit(
            &run_id,
            RunEvent::Started {
                run_id: run_id.clone(),
                goal: goal.to_string(),
                project_root: project_root.to_string(),
                base_commit: base_commit.clone(),
            },
        )
        .await;

        let ctx = PlanContext {
            project_root: project_root.to_string(),
            repo_summary: repo_summary(&repo),
            // Left empty deliberately: enumerating tests means running the test binary, which for
            // an unknown repository means running whatever its build does. The validator degrades
            // to accepting any identifier rather than skipping validation, and that trade is
            // recorded here rather than hidden inside the validator.
            known_tests: Vec::new(),
        };

        let mut graph = self.plan(&wf, goal, &ctx).await?;

        // Work that has already been accepted, kept across a redraft.
        //
        // A new graph does not invalidate a task that passed its own acceptance check under the old
        // one, and throwing that away would make every ownership violation cost the whole run. A
        // task is carried over only when the new graph still contains its id — a planner that
        // dropped or renamed it has said it is no longer part of the plan.
        let mut accepted: HashMap<String, Option<String>> = HashMap::new();
        let mut attempt: u32 = 0;
        let (validated, queue, failed) = loop {
        let validated = validate(&graph, &AnyTest)
            .map_err(|problems| anyhow!("the graph did not validate: {problems:?}"))?;

        let summaries: Vec<TaskSummary> = validated
            .tasks
            .iter()
            .map(|t| TaskSummary {
                id: t.id.clone(),
                title: t.title.clone(),
                depends_on: t.depends_on.clone(),
                declared_paths: t.declared_paths.clone(),
                verify_cmd: t.verify.cmd.clone(),
                must_pass: t.verify.must_pass.clone(),
            })
            .collect();

        self.emit(
            &run_id,
            RunEvent::Planned {
                tasks: summaries,
                waves: validated.waves.clone(),
                attempt,
            },
        )
        .await;
        wf.set_status(wkbd_orch::RunStatus::Running).await?;

        let by_id: HashMap<&str, &DraftTask> =
            validated.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

        let mut commits: HashMap<String, String> = HashMap::new();
        let mut queue: Vec<QueueEntry> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut violations: Vec<(String, String)> = Vec::new();

        // Everything carried over from a previous attempt counts as done before the waves start, so
        // its dependents can proceed and it is not run a second time.
        for (task_id, commit) in &accepted {
            match commit {
                Some(c) => {
                    commits.insert(task_id.clone(), c.clone());
                    queue.push(QueueEntry { task_id: task_id.clone(), commit: c.clone() });
                }
                None => {
                    commits.insert(task_id.clone(), base_commit.clone());
                }
            }
        }

        for wave in &validated.waves {
            // Checked at the wave boundary and again before each dispatch. A cancellation that is
            // only honoured between waves still starts every task in the current one, which for a
            // wide graph is most of the run.
            if self.is_cancelled(&run_id) {
                return Ok(());
            }

            // Everything in one wave at once. The waves come from the edges, so this is the whole
            // parallelism story: no heuristic decides what is safe to run together, the graph does.
            let mut handles = Vec::new();
            for task_id in wave {
                if self.is_cancelled(&run_id) {
                    break;
                }
                let Some(task) = by_id.get(task_id.as_str()).copied() else { continue };
                if accepted.contains_key(task_id) {
                    continue;
                }

                // A task whose dependency failed cannot start: its starting point would be missing
                // the work it was built on. Blocked rather than failed — nothing is wrong with the
                // task itself, and the distinction is what tells the reader where to look.
                if task.depends_on.iter().any(|d| failed.contains(d)) {
                    self.emit(
                        &run_id,
                        RunEvent::TaskStateChanged {
                            task_id: task.id.clone(),
                            status: TaskStatus::Blocked,
                            detail: Some(format!(
                                "a dependency failed: {}",
                                task.depends_on
                                    .iter()
                                    .filter(|d| failed.contains(d))
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )),
                        },
                    )
                    .await;
                    failed.push(task.id.clone());
                    continue;
                }

                let me = self.clone();
                let wf2 = Workflow::resume(&self.store, &run_id);
                let task = task.clone();
                let repo2 = repo.clone();
                let base2 = base_commit.clone();
                let deps: HashMap<String, String> = commits.clone();
                handles.push(tokio::spawn(async move {
                    let id = task.id.clone();
                    let outcome = me.run_task(&wf2, &repo2, &task, &base2, &deps).await;
                    (id, outcome)
                }));
            }

            for handle in handles {
                let (task_id, outcome) = match handle.await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "a task panicked");
                        continue;
                    }
                };
                match outcome {
                    Ok(TaskOutcome::LeftItsPaths { trigger }) => {
                        violations.push((task_id.clone(), trigger));
                    }
                    Ok(TaskOutcome::Produced(Some(commit))) => {
                        commits.insert(task_id.clone(), commit.clone());
                        queue.push(QueueEntry { task_id: task_id.clone(), commit });
                        self.emit(
                            &run_id,
                            RunEvent::TaskStateChanged {
                                task_id,
                                status: TaskStatus::Completed,
                                detail: None,
                            },
                        )
                        .await;
                    }
                    Ok(TaskOutcome::Produced(None)) => {
                        // Accepted, changed nothing. It contributes no commit, so it is not in the
                        // merge queue, but it did not fail and its dependents can still start.
                        commits.insert(task_id.clone(), base_commit.clone());
                        self.emit(
                            &run_id,
                            RunEvent::TaskStateChanged {
                                task_id,
                                status: TaskStatus::Completed,
                                detail: Some("changed nothing".into()),
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        failed.push(task_id.clone());
                        self.emit(
                            &run_id,
                            RunEvent::TaskStateChanged {
                                task_id,
                                status: TaskStatus::Failed,
                                detail: Some(e.to_string()),
                            },
                        )
                        .await;
                    }
                }
            }
        }

        // What survives into the next attempt.
        //
        // A task that produced a commit keeps it; one that was accepted having changed nothing is
        // remembered as such, so its dependents still start. A task that failed is left out, because
        // the point of redrafting is to give it a different shape.
        for task_id in commits.keys() {
            if failed.contains(task_id) {
                continue;
            }
            let commit = queue
                .iter()
                .find(|e| &e.task_id == task_id)
                .map(|e| e.commit.clone());
            accepted.entry(task_id.clone()).or_insert(commit);
        }

        if violations.is_empty() || attempt >= MAX_REPLANS || self.is_cancelled(&run_id) {
            break (validated, queue, failed);
        }

        // The second point at which the model is allowed to decide anything.
        //
        // Handed the graph it produced and the violation as a problem, exactly as a rejected draft
        // is. Re-deriving the schedule is the whole reason this is not just a wider declaration on
        // the offending task: overlapping declarations are what force two tasks to run in sequence,
        // so a task that took paths it did not declare may now belong in a different wave from the
        // one it ran in.
        attempt += 1;
        for (task_id, trigger) in &violations {
            self.emit(
                &run_id,
                RunEvent::Replanning {
                    trigger: trigger.clone(),
                    task_id: task_id.clone(),
                    attempt,
                },
            )
            .await;
        }

        let problems: Vec<String> = violations
            .iter()
            .map(|(task, trigger)| {
                format!(
                    "task {task:?} changed files it did not declare ({trigger}). Declared paths \
                     decide which tasks may run at the same time, so widen this task's \
                     declared_paths to cover what it really touches and move it if that now \
                     overlaps another task."
                )
            })
            .collect();

        let previous = graph.clone();
        let key = format!("replan/{attempt}");
        let ctx_ref = &ctx;
        let redrafted: DraftGraph = wf
            .step(&key, || async move {
                self.planner.redraft(goal, ctx_ref, &previous, &problems).await
            })
            .await
            .context("redrafting after an ownership violation")?;
        graph = redrafted;
        };

        if self.is_cancelled(&run_id) {
            // No candidate is assembled for a cancelled run. Presenting one would invite a merge of
            // a partial result the user stopped on purpose, and the tasks that did finish keep their
            // branches either way.
            return Ok(());
        }

        if queue.is_empty() {
            self.emit(
                &run_id,
                RunEvent::Finished {
                    status: if failed.is_empty() { RunStatus::Done } else { RunStatus::Failed },
                    detail: Some(if failed.is_empty() {
                        "no task produced a change".into()
                    } else {
                        format!("every task failed: {}", failed.join(", "))
                    }),
                },
            )
            .await;
            wf.set_status(if failed.is_empty() {
                wkbd_orch::RunStatus::Done
            } else {
                wkbd_orch::RunStatus::Failed
            })
            .await?;
            // A run where everything failed is the one with the most to learn from, so this is not
            // conditional on success.
            crate::learn::spawn(self.store.clone(), run_id.clone(), project_root.to_string());
            return Ok(());
        }

        let order: Vec<String> = validated
            .topological_order
            .iter()
            .filter(|id| queue.iter().any(|e| &&e.task_id == id))
            .cloned()
            .collect();

        // Assembling the candidate is checkpointed: it creates commits, and doing it twice after a
        // crash would leave the first set unreferenced and confuse the log about which candidate
        // the human was shown.
        let repo3 = repo.clone();
        let base3 = base_commit.clone();
        let queue2 = queue.clone();
        let order2 = order.clone();
        let outcome: SerialisableMergeOutcome = wf
            .step("merge/candidate", || async move {
                let o = drain_merge_queue(&repo3, &base3, &queue2, &order2)?;
                Ok(SerialisableMergeOutcome::from(o))
            })
            .await?;

        match outcome {
            SerialisableMergeOutcome::Merged { commit, order } => {
                let excluded: Vec<String> = failed;
                self.emit(
                    &run_id,
                    RunEvent::AwaitingMerge { commit, order, excluded },
                )
                .await;
                wf.set_status(wkbd_orch::RunStatus::AwaitingMerge).await?;
                // Learned from now rather than after the merge. The run is over as far as the work
                // goes, and whether a person accepts the candidate says nothing about which tasks
                // passed their acceptance checks — that is what there is to learn from.
                crate::learn::spawn(
                    self.store.clone(),
                    run_id.clone(),
                    project_root.to_string(),
                );
            }
            SerialisableMergeOutcome::Rejected { task_id, detail, merged } => {
                self.emit(
                    &run_id,
                    RunEvent::MergeRejected { task_id, detail, merged },
                )
                .await;
                wf.set_status(wkbd_orch::RunStatus::Failed).await?;
                crate::learn::spawn(
                    self.store.clone(),
                    run_id.clone(),
                    project_root.to_string(),
                );
            }
        }

        Ok(())
    }

    /// Drafts and validates, redrafting while the validator keeps saying no.
    async fn plan(&self, wf: &Workflow, goal: &str, ctx: &PlanContext) -> Result<DraftGraph> {
        let run_id = wf.run_id().to_string();
        let mut previous: Option<DraftGraph> = None;
        let mut problems: Vec<String> = Vec::new();

        for attempt in 0..MAX_PLAN_ATTEMPTS {
            // Checkpointed per attempt. A crash between the draft and its validation must not ask
            // the model again: the same prompt does not produce the same graph, and the log would
            // then describe a graph the run never used.
            let key = format!("plan/{attempt}");
            let prev_for_step = previous.clone();
            let problems_for_step = problems.clone();
            let graph: DraftGraph = wf
                .step(&key, || async {
                    match &prev_for_step {
                        None => self.planner.draft(goal, ctx).await,
                        Some(p) => {
                            self.planner.redraft(goal, ctx, p, &problems_for_step).await
                        }
                    }
                })
                .await?;

            match validate(&graph, &AnyTest) {
                Ok(_) => return Ok(graph),
                Err(found) => {
                    problems = describe_problems(&found);
                    self.emit(
                        &run_id,
                        RunEvent::PlanRejected { problems: problems.clone(), attempt },
                    )
                    .await;
                    previous = Some(graph);
                }
            }
        }

        Err(anyhow!(
            "the planner produced {MAX_PLAN_ATTEMPTS} graphs that did not validate; the last \
             problems were: {}",
            problems.join("; ")
        ))
    }

    /// One task, from empty worktree to a verified commit.
    async fn run_task(
        self: &Arc<Self>,
        wf: &Workflow,
        repo: &Path,
        task: &DraftTask,
        base_commit: &str,
        dependency_commits: &HashMap<String, String>,
    ) -> Result<TaskOutcome> {
        let run_id = wf.run_id().to_string();

        self.emit(
            &run_id,
            RunEvent::TaskStateChanged {
                task_id: task.id.clone(),
                status: TaskStatus::Ready,
                detail: None,
            },
        )
        .await;

        let ws_root = self.worktree_root.join(&run_id);
        std::fs::create_dir_all(&ws_root).ok();

        let repo2 = repo.to_path_buf();
        let task2 = task.clone();
        let base2 = base_commit.to_string();
        let deps2 = dependency_commits.clone();
        let run_for_branch = run_id.clone();
        let ws: WorkspaceRecord = wf
            .step(&format!("task/{}/workspace", task.id), || async move {
                let w =
                    prepare_workspace(&repo2, &ws_root, &run_for_branch, &task2, &base2, &deps2)?;
                Ok(WorkspaceRecord {
                    task_id: w.task_id,
                    branch: w.branch,
                    path: w.path.display().to_string(),
                    start_commit: w.start_commit,
                })
            })
            .await
            .with_context(|| format!("preparing a workspace for {}", task.id))?;

        self.emit(
            &run_id,
            RunEvent::TaskWorkspaceReady {
                task_id: task.id.clone(),
                branch: ws.branch.clone(),
                start_commit: ws.start_commit.clone(),
                from_dependencies: task
                    .depends_on
                    .iter()
                    .filter_map(|d| dependency_commits.get(d).cloned())
                    .collect(),
            },
        )
        .await;

        // The tests are made unwritable before the agent starts. Not advisory: an agent that can
        // edit its own acceptance test has no acceptance test, and audits of exactly this setup
        // found frontier models doing it the majority of the time.
        let locked = wkbd_vcs::immutable::snapshot_paths(
            Path::new(&ws.path),
            &task.verify.immutable_paths,
        )
        .ok();
        let _ = wkbd_vcs::immutable::lock_paths(Path::new(&ws.path), &task.verify.immutable_paths);

        // Which agent, and why it is recorded.
        //
        // A choice nobody can see is a choice nobody can question, and this one is made by a model of
        // past outcomes rather than by the user. The decision id is carried to the reward below so
        // that what the router learns is tied to this task's acceptance result and to nothing else.
        let decision = match &self.routing {
            Some(routing) => routing.choose(task, 0).await,
            None => None,
        };
        let agent = decision
            .as_ref()
            .map(|d| d.arm.clone())
            .unwrap_or_else(|| self.worker_agent.clone());

        self.emit(
            &run_id,
            RunEvent::TaskStateChanged {
                task_id: task.id.clone(),
                status: TaskStatus::Dispatched,
                detail: Some(match &decision {
                    Some(_) => format!("routed to {agent}"),
                    None => format!("dispatched to {agent}"),
                }),
            },
        )
        .await;

        let agent_for_step = agent.clone();
        let result: TaskResult = wf
            .step(&format!("task/{}/dispatch", task.id), || async {
                self.dispatch(task, &ws, &agent_for_step).await
            })
            .await
            .with_context(|| format!("dispatching {}", task.id))?;

        // Unlocked before verification, because the test command legitimately writes into the tree
        // (build artefacts, caches) and a read-only test directory fails for the wrong reason.
        let _ = wkbd_vcs::immutable::unlock_paths(
            Path::new(&ws.path),
            &task.verify.immutable_paths,
        );
        if let Some(snapshot) = &locked {
            // Restoring rather than only checking: an agent that edited a test has already
            // invalidated its own acceptance, and leaving the edit in place means verification
            // measures the agent's tests instead of ours.
            match wkbd_vcs::immutable::verify_snapshot(Path::new(&ws.path), snapshot) {
                Ok(report)
                    if !report.modified.is_empty()
                        || !report.missing.is_empty()
                        || !report.replaced_by_symlink.is_empty() =>
                {
                    let _ = wkbd_vcs::immutable::restore_from_snapshot(
                        Path::new(&ws.path),
                        snapshot,
                    );
                    self.emit(
                        &run_id,
                        RunEvent::TaskStateChanged {
                            task_id: task.id.clone(),
                            status: TaskStatus::Verifying,
                            detail: Some(format!(
                                "the agent changed protected paths and they were restored: {:?}",
                                report.modified
                            )),
                        },
                    )
                    .await;
                }
                _ => {}
            }
        }

        if let Some(trigger) = check_ownership(Path::new(&ws.path), task, &ws.start_commit)? {
            // A declaration is a scheduling input, not a note: overlapping declarations are what
            // force two tasks to run in sequence. So a task that wandered outside its own has not
            // just broken a rule, it has invalidated the reasoning the whole schedule was built on —
            // which is why the answer is a new graph rather than a wider declaration for this task.
            // Widening it here and carrying on would leave it running concurrently with whatever now
            // shares the paths it took.
            return Ok(TaskOutcome::LeftItsPaths { trigger: format!("{trigger:?}") });
        }

        self.emit(
            &run_id,
            RunEvent::TaskStateChanged {
                task_id: task.id.clone(),
                status: TaskStatus::Verifying,
                detail: None,
            },
        )
        .await;

        let ws_path = ws.path.clone();
        let spec = task.verify.clone();
        let record: VerifyRecord = wf
            .step_with_retry(
                &format!("task/{}/verify", task.id),
                RetryPolicy::default(),
                |_attempt| {
                    let ws_path = ws_path.clone();
                    let spec = spec.clone();
                    async move {
                        let parser = CargoTestParser;
                        let verifier = Verifier {
                            spec: &spec,
                            parser: &parser,
                            timeout: std::time::Duration::from_secs(600),
                        };
                        let report = verifier.run(Path::new(&ws_path), "")?;
                        // The two failure lists are pulled out of the outcome rather than
                        // flattened into "failed". A change that makes one test pass and breaks two
                        // is not an improvement, and a reader who only sees "failed" cannot tell
                        // which of the two happened.
                        let (missing_pass, regressed) = match &report.outcome {
                            VerifyOutcome::Failed { missing_pass, regressed } => {
                                (missing_pass.clone(), regressed.clone())
                            }
                            _ => (Vec::new(), Vec::new()),
                        };
                        Ok(VerifyRecord {
                            passed: report.outcome.passed(),
                            missing_pass,
                            regressed,
                            detail: Some(describe_outcome(&report.outcome)),
                        })
                    }
                },
            )
            .await
            .with_context(|| format!("verifying {}", task.id))?;

        self.emit(
            &run_id,
            RunEvent::TaskVerified {
                task_id: task.id.clone(),
                passed: record.passed,
                missing_pass: record.missing_pass.clone(),
                regressed: record.regressed.clone(),
                detail: record.detail.clone(),
            },
        )
        .await;

        // The acceptance check is the whole signal. The strong results for learning from experience
        // rest on having a verifiable one, and where there is none the reported benefit collapses to
        // roughly nothing — so nothing here grades quality and no partial credit is invented for a
        // task that failed in an interesting way.
        if let (Some(routing), Some(decision)) = (&self.routing, &decision) {
            let cost = routing.cost_of(&decision.arm);
            routing.reward(&decision.id, record.passed, cost).await;
        }

        if !record.passed {
            return Err(anyhow!(
                "{} did not pass acceptance: {} did not start passing, {} regressed",
                task.id,
                record.missing_pass.len(),
                record.regressed.len()
            ));
        }

        Ok(TaskOutcome::Produced(result.commit))
    }

    /// Runs one agent session in the task's worktree and commits what it produced.
    async fn dispatch(
        &self,
        task: &DraftTask,
        ws: &WorkspaceRecord,
        agent_id: &str,
    ) -> Result<TaskResult> {
        let session = self
            .state
            .open_session(
                agent_id,
                &ws.path,
                wkbd_agent::SessionPurpose::OrchestratorWorker,
            )
            .await
            .context("opening a worker session")?;

        let state = self.state.clone();
        let publisher = state.clone();
        let ctx = crate::runner::TurnContext {
            store: self.store.clone(),
            publish: Arc::new(move |events: &[Event]| publisher.publish(events)),
            session_local_id: session.handle.local_id.clone(),
            handle: session.handle.clone(),
            permissions: state.permissions.clone(),
            project_root: session.project_root.clone(),
            // A worker's permission requests have nobody watching them. Waiting for a human would
            // stall every parallel run on the first tool call; refusing everything would make the
            // worker useless. So they are answered automatically and recorded, and the boundary
            // that actually constrains the worker is the worktree plus the path guard rooted at it
            // — not the prompt.
            ask_user: Arc::new(crate::runner::AutoAllow),
            guard: session.guard.clone(),
        };

        let prompt = task_prompt(task);
        let stop = {
            let mut inbox = session.inbox.lock().await;
            crate::runner::run_turn(&ctx, &prompt, &mut inbox).await?
        };

        let commit = wkbd_vcs::worktree::commit_all(
            Path::new(&ws.path),
            &format!("{}: {}", task.id, task.title),
            &wkbd_vcs::merge::Identity {
                name: "wkbd worker".to_string(),
                email: "worker@wkbd.invalid".to_string(),
            },
        )
        .with_context(|| format!("committing the result of {}", task.id))?;

        Ok(TaskResult {
            task_id: task.id.clone(),
            commit,
            stop_reason: format!("{stop:?}"),
        })
    }

    /// Merges a candidate into the project's checked-out branch. Only ever called by a human's
    /// explicit request.
    pub async fn merge(&self, run_id: &str) -> Result<String> {
        let (_, project_root) = self.run_row(run_id).await?;
        let commit = self
            .candidate_commit(run_id)
            .await?
            .ok_or_else(|| anyhow!("this run has no merge candidate"))?;

        let repo = PathBuf::from(&project_root);
        let head = wkbd_vcs::head_commit(&repo)?;

        // Re-predicted at merge time rather than trusting the candidate. The branch can have moved
        // since the candidate was built — somebody committed while the run was going — and merging
        // a candidate whose base is stale silently reverts their work.
        let prediction = wkbd_vcs::merge::predict_merge(&repo, &head, &commit)?;
        if !prediction.clean {
            return Err(anyhow!(
                "the candidate no longer merges cleanly into {head}: {}",
                prediction
                    .conflicted_paths
                    .iter()
                    .map(|p| p.path.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let merged = wkbd_vcs::merge::create_integration_commit(
            &repo,
            &[head.as_str(), commit.as_str()],
            &format!("wkbd run {run_id}"),
        )?;
        wkbd_vcs::update_head(&repo, &merged)?;

        Workflow::resume(&self.store, run_id)
            .set_status(wkbd_orch::RunStatus::Done)
            .await?;
        self.emit(
            run_id,
            RunEvent::Finished {
                status: RunStatus::Done,
                detail: Some(format!("merged as {merged}")),
            },
        )
        .await;
        Ok(merged)
    }

    /// Abandons a run's candidate. The commits stay in the object database — they are reachable
    /// from the task branches — so this is a decision not to merge rather than a deletion.
    pub async fn abandon(&self, run_id: &str) -> Result<()> {
        Workflow::resume(&self.store, run_id)
            .set_status(wkbd_orch::RunStatus::Cancelled)
            .await?;
        self.emit(
            run_id,
            RunEvent::Finished {
                status: RunStatus::Cancelled,
                detail: Some("abandoned without merging".into()),
            },
        )
        .await;
        Ok(())
    }

    async fn candidate_commit(&self, run_id: &str) -> Result<Option<String>> {
        let wf = Workflow::resume(&self.store, run_id);
        let Some(raw) = wf.checkpoint("merge/candidate").await? else { return Ok(None) };
        let outcome: SerialisableMergeOutcome = serde_json::from_str(&raw)?;
        Ok(match outcome {
            SerialisableMergeOutcome::Merged { commit, .. } => Some(commit),
            SerialisableMergeOutcome::Rejected { .. } => None,
        })
    }

    async fn run_row(&self, run_id: &str) -> Result<(String, String)> {
        let id = run_id.to_string();
        self.store
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT goal, project_root FROM runs WHERE id = ?1",
                    [&id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )?)
            })
            .await
    }

    /// Appends a run event to the log and publishes it.
    ///
    /// Same log as conversation events, under the stream id `run:<uuid>`. One log means one
    /// ordering and one resume mechanism; a separate table would need its own answer to "what
    /// happened, in what order", and the two answers would disagree.
    async fn emit(&self, run_id: &str, event: RunEvent) {
        let stream = run_stream_id(run_id);
        let pending = PendingEvent::new(stream, EventPayload::Run { run: event });
        match self.store.append(vec![pending]).await {
            Ok(written) => self.state.publish(&written),
            Err(e) => {
                // A failed append must not stop a run. Worktrees exist and commits have been made;
                // losing a log line is much less bad than abandoning the work.
                tracing::error!(run = %run_id, error = %e, "could not record a run event");
            }
        }
    }
}

/// `MergeQueueOutcome` with a serde representation, so it can be a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum SerialisableMergeOutcome {
    Merged { commit: String, order: Vec<String> },
    Rejected { task_id: String, detail: String, merged: Vec<String> },
}

impl From<MergeQueueOutcome> for SerialisableMergeOutcome {
    fn from(o: MergeQueueOutcome) -> Self {
        match o {
            MergeQueueOutcome::Merged { commit, order } => {
                SerialisableMergeOutcome::Merged { commit, order }
            }
            MergeQueueOutcome::Rejected { task_id, detail, retried_without } => {
                SerialisableMergeOutcome::Rejected { task_id, detail, merged: retried_without }
            }
        }
    }
}

/// What a worker is told.
///
/// The acceptance criterion is included verbatim. A worker that does not know which tests decide
/// its fate optimises for looking finished, and the protected paths are stated because discovering
/// them by hitting a read-only file produces a confusing failure rather than a decision.
pub fn task_prompt(task: &DraftTask) -> String {
    let mut s = format!("{}\n\n{}\n", task.title, task.body);
    if !task.declared_paths.is_empty() {
        s.push_str(&format!(
            "\nChange only these paths: {}. Changing anything else fails this task.\n",
            task.declared_paths.join(", ")
        ));
    }
    if !task.verify.must_pass.is_empty() {
        s.push_str(&format!(
            "\nThese must pass when you are done: {}\n",
            task.verify.must_pass.join(", ")
        ));
    }
    if !task.verify.must_still_pass.is_empty() {
        s.push_str(&format!(
            "These are passing now and must keep passing: {}\n",
            task.verify.must_still_pass.join(", ")
        ));
    }
    if !task.verify.immutable_paths.is_empty() {
        s.push_str(&format!(
            "\nThese are read-only and will be restored if you change them: {}\n",
            task.verify.immutable_paths.join(", ")
        ));
    }
    s
}

/// A one-line description of an acceptance outcome.
///
/// Distinguishes the four ways a task can fail to be accepted, because they call for different
/// responses: a broken environment is not the agent's mistake, a tampered test is a security event
/// rather than a test failure, and an inconclusive run means the parser could not read the output
/// at all — which is a defect in us, not in the agent.
fn describe_outcome(outcome: &VerifyOutcome) -> String {
    match outcome {
        VerifyOutcome::Passed => "passed".into(),
        VerifyOutcome::Failed { missing_pass, regressed } => format!(
            "{} did not start passing, {} regressed",
            missing_pass.len(),
            regressed.len()
        ),
        VerifyOutcome::SetupFailed { command, code, .. } => format!(
            "the environment could not be prepared: {command:?} exited with {code:?}; this is not              the agent's failure"
        ),
        VerifyOutcome::Inconclusive { reason } => {
            format!("nothing could be concluded from the test output: {reason}")
        }
        VerifyOutcome::Tampered { paths } => {
            format!("protected files were modified: {}", paths.join(", "))
        }
    }
}

/// A few lines about the repository for the planner.
///
/// Small on purpose. A planner handed the whole tree spends its attention on the tree.
fn repo_summary(repo: &Path) -> String {
    let mut lines = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() {
                    format!("{n}/")
                } else {
                    n
                }
            })
            .collect();
        names.sort();
        names.truncate(60);
        lines.push(format!("Top level: {}", names.join(" ")));
    }
    for candidate in ["README.md", "Cargo.toml", "package.json", "pyproject.toml"] {
        let p = repo.join(candidate);
        if let Ok(body) = std::fs::read_to_string(&p) {
            let head: String = body.lines().take(20).collect::<Vec<_>>().join("\n");
            lines.push(format!("--- {candidate} (first 20 lines) ---\n{head}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wkbd_orch::VerifySpec;

    fn task() -> DraftTask {
        DraftTask {
            id: "a".into(),
            title: "do the thing".into(),
            body: "in detail".into(),
            declared_paths: vec!["src/a.rs".into()],
            depends_on: vec![],
            verify: VerifySpec {
                setup: vec![],
                cmd: "cargo test".into(),
                must_pass: vec!["a::b".into()],
                must_still_pass: vec!["c::d".into()],
                immutable_paths: vec!["tests/**".into()],
            },
        }
    }

    /// A worker that does not know which tests decide its fate optimises for looking finished.
    #[test]
    fn a_worker_is_told_the_criterion_that_will_judge_it() {
        let p = task_prompt(&task());
        assert!(p.contains("a::b"), "must_pass has to be in the prompt");
        assert!(p.contains("c::d"), "must_still_pass has to be in the prompt");
    }

    #[test]
    fn a_worker_is_told_which_paths_are_read_only_rather_than_discovering_it_by_failing() {
        let p = task_prompt(&task());
        assert!(p.contains("tests/**"));
        assert!(p.contains("read-only"));
    }

    #[test]
    fn a_worker_is_told_the_boundary_it_will_be_checked_against() {
        let p = task_prompt(&task());
        assert!(p.contains("src/a.rs"));
        assert!(p.contains("Change only these paths"));
    }

    /// Empty lists produce no sentence at all rather than "these must pass: ". A prompt with a
    /// dangling empty constraint reads as a constraint the worker cannot satisfy.
    #[test]
    fn absent_constraints_produce_no_sentence() {
        let mut t = task();
        t.declared_paths.clear();
        t.verify.immutable_paths.clear();
        let p = task_prompt(&t);
        assert!(!p.contains("Change only these paths"));
        assert!(!p.contains("read-only"));
    }

    #[test]
    fn the_merge_outcome_survives_a_round_trip_through_a_checkpoint() {
        let o = SerialisableMergeOutcome::from(MergeQueueOutcome::Rejected {
            task_id: "b".into(),
            detail: "conflicts".into(),
            retried_without: vec!["a".into()],
        });
        let json = serde_json::to_string(&o).unwrap();
        let back: SerialisableMergeOutcome = serde_json::from_str(&json).unwrap();
        match back {
            SerialisableMergeOutcome::Rejected { task_id, merged, .. } => {
                assert_eq!(task_id, "b");
                // The tasks that did combine keep their work. Losing this field would turn a
                // partial success into an apparent total failure.
                assert_eq!(merged, vec!["a".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }
}
