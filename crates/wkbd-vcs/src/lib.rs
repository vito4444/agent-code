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
mod pathset;
pub mod worktree;

pub use error::{Result, VcsError};
pub use merge::{create_integration_commit, predict_merge, ConflictedPath, Identity, MergePrediction};
pub use ownership::{changed_paths, changed_paths_including_worktree, check_ownership, OwnershipVerdict};
pub use worktree::{
    add_worktree, hydrate, is_branch_checked_out, list_worktrees, remove_worktree, HydrationSpec,
    SetupCommand, Worktree, WorktreeInfo,
};
