//! Version control for a workbench that runs several coding agents at once.
//!
//! Four things, in dependency order:
//!
//! - [`merge`] — perform a merge in the object database and report whether it conflicts,
//!   without touching any working tree. This is the primitive the orchestrator schedules
//!   on: it is the only authority on whether two agents' results combine, and it is safe
//!   to ask while both agents are still running.
//! - [`worktree`] — give each agent its own checkout, know which branches are in use, and
//!   make a new worktree actually runnable rather than merely populated.
//! - [`ownership`] — check afterwards that a task changed only what it declared. A
//!   scheduling heuristic with an audit attached, never a merge guarantee.
//! - [`immutable`] — keep the acceptance criteria out of the agent's reach, by restoring
//!   them from a snapshot immediately before they are used.
//!
//! Every git behaviour this crate depends on was measured on the target machine rather
//! than read from documentation; the results are in `docs/M0-FINDINGS.md` and can be
//! re-measured with `scripts/m0-mergetree.sh`. Two of them are counter-intuitive enough
//! to be worth repeating here: `merge-tree`'s exit code and its `--stdin` status number
//! mean opposite things, and two branches with no overlapping paths can still fail to
//! merge.

pub mod error;
mod git;
pub mod immutable;
pub mod merge;
pub mod ownership;
pub mod review;
mod pathset;
pub mod worktree;

pub use error::{Result, VcsError};
pub use merge::{create_integration_commit, predict_merge, ConflictedPath, Identity, MergePrediction};
pub use ownership::{changed_paths, changed_paths_including_worktree, check_ownership, OwnershipVerdict};
pub use review::{changes_between, ChangeSet, FileChange};
pub use worktree::{
    add_worktree, hydrate, is_branch_checked_out, list_worktrees, remove_worktree, HydrationSpec,
    SetupCommand, Worktree, WorktreeInfo,
};

/// The commit the repository's checked-out branch is on.
pub fn head_commit(repo: &std::path::Path) -> Result<String> {
    let out = git::run(repo, &["rev-parse", "HEAD"])?;
    if !out.success() {
        return Err(out.error());
    }
    Ok(out.stdout_trimmed()?.to_string())
}

/// Moves the checked-out branch to `commit`, updating the working tree to match.
///
/// `merge --ff-only` rather than `reset --hard`: a fast-forward refuses when the branch has moved
/// somewhere the commit does not contain, which is exactly the case where a reset would silently
/// discard somebody's work. The caller has already predicted the merge, so the refusal here is the
/// second check on a race the first one cannot close — the branch can move between the prediction
/// and this call.
pub fn update_head(repo: &std::path::Path, commit: &str) -> Result<()> {
    let out = git::run(repo, &["merge", "--ff-only", commit])?;
    if !out.success() {
        return Err(out.error());
    }
    Ok(())
}

/// Resolves a revision — a branch name, a tag, a short hash — to a full commit id.
///
/// Errors when it does not name a commit, rather than returning the input. A caller that got its
/// input back would go on to use a branch name where an immutable id was required, and the branch
/// moves.
pub fn resolve(repo: &std::path::Path, rev: &str) -> Result<String> {
    git::resolve_commit(repo, rev)
}
