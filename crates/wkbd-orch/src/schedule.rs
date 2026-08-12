//! Scheduling, integration and merge.
//!
//! No model is involved in anything here. The model produced the graph; from that point the
//! sequence is a topological sort, the isolation is a real worktree, the integration point for a
//! dependent task is a real merge commit, and the decision to replan is a predicate over
//! observable facts.
//!
//! The last of those is the one that is easy to get wrong. A framework that calls the model
//! whenever it is unsure degenerates into a conversation: the documented complaint about the
//! best-known multi-agent frameworks is precisely that they have "no awareness of semantic
//! progress" and fall back on a turn limit. So the triggers here are enumerable machine facts,
//! and each one produces a fixed input template, which is also what makes a replan reproducible.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::graph::{DraftTask, ValidatedGraph};

/// Why the model is being called again.
///
/// Exhaustive and machine-decidable. There is no "the orchestrator felt stuck" variant, because
/// that is how a deterministic pipeline turns back into a chat loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum ReplanTrigger {
    /// Two results that must be combined do not merge. Established by running the merge, not by
    /// comparing declared file lists: paths can be disjoint and still conflict.
    MergeConflict { task: String, with: Vec<String>, messages: Vec<String> },
    /// Acceptance failed after exhausting the retry budget.
    AcceptanceFailed { task: String, attempts: u32, missing_pass: Vec<String> },
    /// The task modified files outside what it declared.
    OwnershipViolation { task: String, unexpected: Vec<String> },
    /// The task's acceptance command exceeded its time budget.
    Timeout { task: String, seconds: u64 },
    /// Files the task was told not to modify were modified.
    Tampering { task: String, paths: Vec<String> },
}

impl ReplanTrigger {
    /// The task that triggered this, for scoping the replan.
    pub fn task(&self) -> &str {
        match self {
            ReplanTrigger::MergeConflict { task, .. }
            | ReplanTrigger::AcceptanceFailed { task, .. }
            | ReplanTrigger::OwnershipViolation { task, .. }
            | ReplanTrigger::Timeout { task, .. }
            | ReplanTrigger::Tampering { task, .. } => task,
        }
    }
}

/// A cheap pre-filter over declared paths.
///
/// Returns pairs that *might* conflict. Nothing decides parallelism from this: on this machine,
/// two branches that touch disjoint paths were measured failing to merge, because a directory
/// rename split conflicts with no individual file conflicting. So this only orders work, and
/// the merge itself decides.
pub fn declared_path_overlaps(tasks: &[DraftTask]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, a) in tasks.iter().enumerate() {
        for b in tasks.iter().skip(i + 1) {
            if a.declared_paths.iter().any(|p| b.declared_paths.contains(p)) {
                out.push((a.id.clone(), b.id.clone()));
            }
        }
    }
    out
}

/// Where a task's worktree lives and what it starts from.
#[derive(Debug, Clone)]
pub struct TaskWorkspace {
    pub task_id: String,
    pub branch: String,
    pub path: PathBuf,
    pub start_commit: String,
}

/// Creates the isolated workspace for a task.
///
/// When the task has dependencies, its starting point is a real merge of their results, produced
/// without touching any working tree. That is what makes a dependency edge transport data: the
/// dependent task's files already contain what its dependencies produced. Passing a summary of
/// the dependency's output in a prompt instead — which is what the well-known frameworks do — is
/// an edge that carries a description of the work rather than the work.
pub fn prepare_workspace(
    repo: &Path,
    worktree_root: &Path,
    task: &DraftTask,
    base_commit: &str,
    dependency_commits: &HashMap<String, String>,
) -> Result<TaskWorkspace, PrepareError> {
    let mut parents: Vec<String> = Vec::new();
    for dep in &task.depends_on {
        match dependency_commits.get(dep) {
            Some(commit) => parents.push(commit.clone()),
            None => {
                return Err(PrepareError::DependencyNotFinished {
                    task: task.id.clone(),
                    dependency: dep.clone(),
                })
            }
        }
    }

    let start_commit = if parents.is_empty() {
        base_commit.to_string()
    } else if parents.len() == 1 {
        parents.remove(0)
    } else {
        // More than one dependency: fold them pairwise, predicting each step. A conflict here is
        // discovered before any agent starts, and no working tree has been touched.
        let all: Vec<&str> = parents.iter().map(|s| s.as_str()).collect();
        match wkbd_vcs::merge::create_integration_commit(
            repo,
            &all,
            &format!("integration for {}", task.id),
        ) {
            Ok(commit) => commit,
            Err(e) => {
                return Err(PrepareError::Conflict {
                    task: task.id.clone(),
                    with: task.depends_on.clone(),
                    detail: e.to_string(),
                })
            }
        }
    };

    let branch = format!("wkbd/{}", task.id);
    let path = worktree_root.join(&task.id);

    // Git refuses to check out a branch that another worktree already has. That refusal is
    // useful: it is git enforcing, on our behalf, the rule that a branch in use elsewhere must
    // not be rewritten. Both git upstream and the best-known stacked-branch tool arrived at the
    // same rule independently.
    if let Ok(Some(existing)) = wkbd_vcs::worktree::is_branch_checked_out(repo, &branch) {
        return Err(PrepareError::BranchBusy { branch, held_by: existing });
    }

    wkbd_vcs::worktree::add_worktree(repo, &path, &branch, &start_commit)
        .map_err(|e| PrepareError::Worktree { task: task.id.clone(), detail: e.to_string() })?;

    Ok(TaskWorkspace { task_id: task.id.clone(), branch, path, start_commit })
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("task {task} depends on {dependency}, which has not produced a commit yet")]
    DependencyNotFinished { task: String, dependency: String },
    #[error("task {task} cannot start: its dependencies {with:?} do not merge: {detail}")]
    Conflict { task: String, with: Vec<String>, detail: String },
    #[error("branch {branch} is already checked out at {held_by}")]
    BranchBusy { branch: String, held_by: PathBuf },
    #[error("could not create a worktree for {task}: {detail}")]
    Worktree { task: String, detail: String },
}

/// One entry in the merge queue.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub task_id: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueOutcome {
    /// Everything combined and the result is this commit.
    Merged { commit: String, order: Vec<String> },
    /// One entry could not be combined. Only that entry is removed; the rest continue.
    Rejected { task_id: String, detail: String, retried_without: Vec<String> },
}

/// Combines finished tasks in topological order.
///
/// Two properties borrowed from how a merge queue works, both of which matter more than they
/// look:
///
/// - **What gets validated is the combined result, not each branch.** Two changes that are each
///   fine and together are not is the entire reason a queue exists.
/// - **A failure removes only its own entry, and the rest are rebuilt without it.** Rolling the
///   whole batch back throws away work that was fine, and re-running it produces the same
///   combination and the same failure.
pub fn drain_merge_queue(
    repo: &Path,
    base: &str,
    entries: &[QueueEntry],
    order: &[String],
) -> Result<MergeQueueOutcome> {
    let by_id: HashMap<&str, &QueueEntry> =
        entries.iter().map(|e| (e.task_id.as_str(), e)).collect();

    let mut accumulated = base.to_string();
    let mut merged_order = Vec::new();

    for task_id in order {
        let Some(entry) = by_id.get(task_id.as_str()) else { continue };

        match wkbd_vcs::merge::predict_merge(repo, &accumulated, &entry.commit) {
            Ok(prediction) if prediction.clean => {
                accumulated = wkbd_vcs::merge::create_integration_commit(
                    repo,
                    &[accumulated.as_str(), entry.commit.as_str()],
                    &format!("merge {task_id}"),
                )?;
                merged_order.push(task_id.clone());
            }
            Ok(prediction) => {
                let detail = if prediction.conflicted_paths.is_empty() {
                    // A conflict with no individual conflicting file. This is the case that
                    // makes "the paths do not overlap" an unsafe conclusion, and it is why the
                    // message names the mechanism rather than a file.
                    format!(
                        "conflicts with the accumulated result with no single conflicting file: {}",
                        prediction.messages.join("; ")
                    )
                } else {
                    format!(
                        "conflicting files: {}",
                        prediction
                            .conflicted_paths
                            .iter()
                            .map(|p| p.path.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return Ok(MergeQueueOutcome::Rejected {
                    task_id: task_id.clone(),
                    detail,
                    retried_without: merged_order,
                });
            }
            Err(e) => {
                return Ok(MergeQueueOutcome::Rejected {
                    task_id: task_id.clone(),
                    detail: e.to_string(),
                    retried_without: merged_order,
                })
            }
        }
    }

    Ok(MergeQueueOutcome::Merged { commit: accumulated, order: merged_order })
}

/// Checks a finished task against what it said it would touch.
///
/// Costs one `git diff` and catches the most common planning mistake. The declaration is not
/// trusted for correctness — see [`declared_path_overlaps`] — but a task that wandered outside
/// its declaration has invalidated the reasoning the schedule was built on, so it is a hard
/// failure rather than a warning.
pub fn check_ownership(
    worktree: &Path,
    task: &DraftTask,
    start_commit: &str,
) -> Result<Option<ReplanTrigger>> {
    let actual = wkbd_vcs::ownership::changed_paths_including_worktree(worktree, start_commit)?;
    let verdict = wkbd_vcs::ownership::check_ownership(&task.declared_paths, &actual);
    if verdict.violations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ReplanTrigger::OwnershipViolation {
            task: task.id.clone(),
            unexpected: verdict.violations,
        }))
    }
}

/// The tasks ready to dispatch: everything whose dependencies have all finished.
pub fn ready_tasks(graph: &ValidatedGraph, finished: &[String]) -> Vec<String> {
    graph
        .tasks
        .iter()
        .filter(|t| !finished.contains(&t.id))
        .filter(|t| t.depends_on.iter().all(|d| finished.contains(d)))
        .map(|t| t.id.clone())
        .collect()
}
