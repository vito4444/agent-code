//! After-the-fact check: did this task only touch what it said it would touch?
//!
//! The orchestrator asks the planning model to declare, per task, which files the task
//! will change. That declaration is useful for scheduling — two tasks whose declared sets
//! overlap are not worth starting in parallel — and it is worth nothing as a guarantee.
//! Models are wrong about their own future edits routinely, and nothing stops an agent
//! from editing outside its declaration.
//!
//! So the declaration is a heuristic, and this module is the audit. It runs after the
//! agent stops, it is a couple of `git diff` invocations, and it catches the single most
//! common planning error: a task that quietly grew into another task's files. Cheap
//! enough to run on every task, which is the only reason it is worth having.
//!
//! **This does not replace merge prediction, and it cannot.** Measured
//! (docs/M0-FINDINGS.md §1.3): two branches with an empty path intersection can still
//! fail to merge — a directory rename split conflicts with no conflicting file at all.
//! Reading "declared sets are disjoint" as "these can be merged" is exactly the mistake
//! that reading `merge-tree`'s exit code prevents. Ownership answers "did the agent do
//! what it claimed"; only `crate::merge::predict_merge` answers "do these combine".

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::git;
use crate::pathset::Pattern;

/// Paths changed between two commits, relative to the repository root.
///
/// Rename detection is disabled so a rename is reported as its old path *and* its new
/// path. With detection on, git reports a rename under the destination only, and a task
/// that moved a file out of another task's directory would pass a check it should fail.
pub fn changed_paths(repo_or_worktree: &Path, from_commit: &str, to_commit: &str) -> Result<Vec<String>> {
    let top = git::toplevel(repo_or_worktree)?;
    let out = git::run_checked(
        &top,
        &["diff", "--name-only", "--no-renames", "-z", from_commit, to_commit],
    )?;
    Ok(sorted_unique(out.nul_fields()?))
}

/// Paths changed since `from_commit` including work that is not committed yet.
///
/// Agents are inspected while they are still mid-task, and an agent that has written but
/// not committed has still escaped its declaration. Untracked files are included for the
/// same reason — creating a new file is the easiest way to leave a declared area, and
/// plain `git diff` cannot see one. Ignored files are excluded: build output and local
/// environment files are not the agent making a claim about the codebase.
pub fn changed_paths_including_worktree(worktree: &Path, from_commit: &str) -> Result<Vec<String>> {
    let top = git::toplevel(worktree)?;
    let tracked = git::run_checked(&top, &["diff", "--name-only", "--no-renames", "-z", from_commit])?;
    // `--full-name` because `ls-files` prints paths relative to the current directory,
    // unlike `diff`; without it the two halves of this answer use different bases.
    let untracked = git::run_checked(
        &top,
        &["ls-files", "--others", "--exclude-standard", "--full-name", "-z"],
    )?;

    let mut all = tracked.nul_fields()?;
    all.extend(untracked.nul_fields()?);
    Ok(sorted_unique(all))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipVerdict {
    /// Changed paths covered by some declaration.
    pub within: Vec<String>,
    /// Changed paths covered by none. These are the finding.
    pub violations: Vec<String>,
    /// Declarations that matched nothing. Not a failure — over-declaring is the safe
    /// direction — but it is the signal that the planner's file model is drifting, and
    /// it is free to collect here.
    pub unmatched_declarations: Vec<String>,
    /// Declarations that would not compile as globs, with the reason. A model can emit
    /// anything; see [`check_ownership`] for why this is not an error.
    pub invalid_declarations: Vec<InvalidDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidDeclaration {
    pub declaration: String,
    pub reason: String,
}

impl OwnershipVerdict {
    /// True when every changed path was declared. Invalid declarations do not make a
    /// verdict fail on their own; the paths they were supposed to cover show up as
    /// violations instead, which is the report the user can act on.
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Compares a declaration against what actually changed.
///
/// Infallible on purpose. The inputs are model output and git output, and a single
/// malformed pattern must not destroy the whole verdict — the violations are the thing
/// the user needs to see, and "the check failed to run" is how a check quietly stops
/// being run at all. A pattern that does not compile matches nothing, so anything it was
/// meant to authorise is reported as a violation: wrong in the safe direction, and
/// visible either way.
pub fn check_ownership(declared: &[String], actual: &[String]) -> OwnershipVerdict {
    let mut verdict = OwnershipVerdict::default();
    let mut patterns: Vec<(Pattern, bool)> = Vec::new();

    for declaration in declared {
        match Pattern::parse(declaration) {
            Ok(p) => patterns.push((p, false)),
            Err(e) => verdict.invalid_declarations.push(InvalidDeclaration {
                declaration: declaration.clone(),
                reason: e.to_string(),
            }),
        }
    }

    for path in actual {
        let normalized = path.trim_start_matches("./");
        let mut covered = false;
        for (pattern, used) in patterns.iter_mut() {
            if pattern.is_match(normalized) {
                *used = true;
                covered = true;
            }
        }
        if covered {
            verdict.within.push(path.clone());
        } else {
            verdict.violations.push(path.clone());
        }
    }

    verdict.unmatched_declarations = patterns
        .iter()
        .filter(|(_, used)| !used)
        .map(|(p, _)| p.source.clone())
        .collect();
    // Sorted so a verdict can be compared, diffed and shown without the caller's input
    // order leaking into a report a human is meant to read twice.
    verdict.within.sort();
    verdict.violations.sort();
    verdict
}

fn sorted_unique(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_declarations_cover_their_subtree() {
        let verdict = check_ownership(
            &["src/**/*.rs".to_string(), "Cargo.toml".to_string()],
            &["src/lib.rs".to_string(), "src/deep/mod.rs".to_string(), "Cargo.toml".to_string()],
        );
        assert!(verdict.ok());
        assert!(verdict.unmatched_declarations.is_empty());
    }

    #[test]
    fn reports_which_paths_escaped() {
        let verdict = check_ownership(
            &["src/**".to_string()],
            &["src/lib.rs".to_string(), "tests/it.rs".to_string(), ".github/workflows/ci.yml".to_string()],
        );
        assert!(!verdict.ok());
        assert_eq!(verdict.violations, vec![".github/workflows/ci.yml", "tests/it.rs"]);
        assert_eq!(verdict.within, vec!["src/lib.rs"]);
    }

    #[test]
    fn a_broken_pattern_fails_closed_and_is_named() {
        let verdict = check_ownership(&["../../etc/**".to_string()], &["etc/passwd".to_string()]);
        assert_eq!(verdict.invalid_declarations.len(), 1);
        assert_eq!(verdict.violations, vec!["etc/passwd"]);
    }
}
