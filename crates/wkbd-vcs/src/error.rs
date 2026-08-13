//! One error type for the whole crate.
//!
//! The variants that matter are the ones the orchestrator has to branch on rather than
//! log: a conflicting merge is a normal scheduling outcome (re-plan, re-run, or ask the
//! user), not a failure, so [`VcsError::MergeConflict`] carries the full prediction
//! instead of a rendered string. Likewise the two branch-safety refusals are separated
//! from generic git failures because they are the mechanism that stops one agent
//! rewriting another agent's branch, and swallowing them into "git said no" would hide
//! the one thing the scheduler needs to know.

use std::path::PathBuf;

use crate::merge::MergePrediction;

pub type Result<T> = std::result::Result<T, VcsError>;

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("`git {invocation}` failed ({status}): {stderr}")]
    Git { invocation: String, status: String, stderr: String },

    #[error("could not execute git ({invocation}): {source}")]
    Spawn {
        invocation: String,
        #[source]
        source: std::io::Error,
    },

    /// git produced output we could not parse. Always a bug or a git upgrade that
    /// changed a format we pin; never something a caller can recover from, but the raw
    /// text is attached so the report says which format drifted.
    #[error("unparsable output from `git {invocation}`: {detail}")]
    Parse { invocation: String, detail: String },

    #[error("merging {a} into {b} conflicts in {} path(s)", .prediction.conflicted_paths.len())]
    MergeConflict { a: String, b: String, prediction: Box<MergePrediction> },

    #[error("branch `{branch}` is already checked out at {}", .path.display())]
    BranchAlreadyCheckedOut { branch: String, path: PathBuf },

    #[error("branch `{branch}` already exists")]
    BranchExists { branch: String },

    #[error("worktree at {} has modified or untracked files", .path.display())]
    WorktreeNotClean { path: PathBuf },

    #[error("{operation} failed on {}: {source}", .path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A path or pattern was refused before any filesystem access happened. `reason` is
    /// static text because these are our own rules, not the operating system's.
    #[error("refusing path `{path}`: {reason}")]
    UnsafePath { path: String, reason: &'static str },

    #[error("invalid glob `{pattern}`: {detail}")]
    Glob { pattern: String, detail: String },

    #[error("setup command `{command}` {detail}")]
    SetupCommand { command: String, detail: String },

    #[error("{0}")]
    Invalid(String),
}

impl VcsError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        VcsError::Io { operation, path: path.into(), source }
    }
}
