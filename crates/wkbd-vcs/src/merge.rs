//! Merge prediction: the primitive the whole orchestrator is built on.
//!
//! Two agents finish. Can their results be combined? The only trustworthy answer comes
//! from asking git to actually perform the merge — into the object database, never into
//! a working tree. `git merge-tree --write-tree` does exactly that: it writes the merged
//! tree as objects and reports whether it conflicted, without an index and without
//! touching any checkout. That property is the whole point. Agents keep running while we
//! evaluate combinations of their output; a prediction that dirtied a working tree would
//! corrupt the very work it was evaluating.
//!
//! Three measured facts shape this module (docs/M0-FINDINGS.md §1, reproducible with
//! scripts/m0-mergetree.sh):
//!
//! - **The exit code is the answer.** `0` = clean, `1` = conflict. The tree oid is
//!   printed either way, so the presence of a tree means nothing.
//! - **The `--stdin` batch mode inverts the status number**: there, `1` means clean and
//!   `0` means conflict, and the process exit code is `0` regardless. This crate
//!   deliberately implements single-pair mode only. Supporting both would put two
//!   opposite encodings of "did it conflict" behind one helper, and the day someone
//!   refactors those together every conflicting merge in the system starts reporting
//!   clean. Batch mode buys one process spawn per pair; that is not worth this risk, and
//!   nothing here is spawn-bound.
//! - **Disjoint path sets still conflict.** A directory rename split (one branch splits
//!   `dirA/` into two directories, another adds a file to `dirA/`) conflicts with an
//!   *empty* conflicted-file list — there is no single file to blame. So "no conflicted
//!   files" must never be read as "clean"; only the exit code says that.
//!
//! One more trap, measured here rather than in the findings doc: `merge-tree` also exits
//! **1** when an argument is not a commit-ish (`merge-tree: nosuchref - not something we
//! can merge`, empty stdout). An implementation that maps "exit 1" straight to "conflict"
//! reports a typo as an unmergeable pair forever. Hence the tree oid is validated, and a
//! missing one is an error rather than a conflict.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, VcsError};
use crate::git;

/// One line of the `Conflicted file info` section: `<mode> <object> <stage>\t<path>`.
/// Stages are git's merge stages — 1 = merge base, 2 = "ours", 3 = "theirs" — so a
/// content conflict normally produces three entries for the same path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictedPath {
    pub mode: String,
    pub oid: String,
    pub stage: u8,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePrediction {
    /// Taken from the process exit code and nothing else.
    pub clean: bool,
    /// The merged tree, which exists in the object database even when `clean` is false.
    pub tree_oid: String,
    /// May be empty on a conflicting merge; see the directory-rename-split case above.
    pub conflicted_paths: Vec<ConflictedPath>,
    /// git's informational lines, in order (`Auto-merging x`, `CONFLICT (...): ...`).
    /// Free-form text: fine to show a user, never to branch on.
    pub messages: Vec<String>,
}

impl MergePrediction {
    /// Distinct paths from the conflicted-file section, in first-seen order. Empty on a
    /// clean merge — and, importantly, also empty for some conflicting merges.
    pub fn conflicted_files(&self) -> Vec<&str> {
        let mut seen = Vec::new();
        for entry in &self.conflicted_paths {
            if !seen.contains(&entry.path.as_str()) {
                seen.push(entry.path.as_str());
            }
        }
        seen
    }
}

/// Performs the merge of `a` and `b` in the object database and reports the outcome.
///
/// No working tree, index or ref is touched, so this is safe to call at any time against
/// a repository whose worktrees have running agents in them.
pub fn predict_merge(repo: &Path, a: &str, b: &str) -> Result<MergePrediction> {
    let out = git::run(repo, &["merge-tree", "--write-tree", a, b])?;

    // Anything other than 0 or 1 is git failing rather than answering: unrelated
    // histories exit 128, for example.
    if !matches!(out.code, Some(0) | Some(1)) {
        return Err(out.error());
    }

    let text = out.stdout_utf8()?;
    let parsed = match parse_write_tree(text) {
        Ok(p) => p,
        Err(detail) => {
            // Exit 1 with unparsable output is the "not something we can merge" shape.
            // Report git's own stderr; calling it a conflict would be a lie that the
            // scheduler cannot recover from.
            if out.code != Some(0) {
                return Err(VcsError::Git {
                    invocation: out.invocation.clone(),
                    status: format!("exit code {}", out.code.unwrap_or(-1)),
                    stderr: detail_or_stderr(&out.stderr, &detail),
                });
            }
            return Err(VcsError::Parse { invocation: out.invocation.clone(), detail });
        }
    };

    let clean = out.code == Some(0);
    // Tripwire, not defensive programming: a clean merge with conflicted files would mean
    // the exit code and the output disagree, and every decision in this crate assumes
    // they cannot. Better to stop than to schedule on a contradiction.
    if clean && !parsed.conflicted_paths.is_empty() {
        return Err(VcsError::Parse {
            invocation: out.invocation.clone(),
            detail: format!(
                "exit code 0 (clean) but {} conflicted file entries were printed",
                parsed.conflicted_paths.len()
            ),
        });
    }

    Ok(MergePrediction { clean, ..parsed })
}

/// Builds the commit that carries several agents' results into one, without checking
/// anything out.
///
/// This is what makes a dependency edge in the task graph transport data rather than a
/// sentence in a prompt: the dependent task's worktree is created from the returned
/// commit, so every dependency's output is already present in its files.
///
/// With more than two parents the merge is folded pairwise in the given order, and every
/// step is predicted before it is committed — a pairwise-clean sequence is not implied by
/// any of the individual pairs being clean. Octopus merges are not used: `merge-tree`
/// takes exactly two commits, and `git merge -s octopus` needs an index and a working
/// tree, which is the one thing this path must never require.
pub fn create_integration_commit(repo: &Path, parents: &[&str], message: &str) -> Result<String> {
    create_integration_commit_as(repo, parents, message, &Identity::workbench())
}

/// Who integration commits are attributed to.
///
/// Defaulted rather than inherited on purpose. These commits are made by the machine,
/// and attributing them to whoever happens to be configured in `user.email` makes an
/// automated merge indistinguishable from a human one in `git log`. It also removes a
/// failure mode: `commit-tree` aborts with "Author identity unknown" on a repository
/// with no configured identity, which would otherwise turn a fresh clone into a
/// mysterious orchestration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    pub fn workbench() -> Self {
        Identity {
            name: "wkbd integration".to_string(),
            email: "integration@wkbd.invalid".to_string(),
        }
    }
}

pub fn create_integration_commit_as(
    repo: &Path,
    parents: &[&str],
    message: &str,
    identity: &Identity,
) -> Result<String> {
    if parents.is_empty() {
        return Err(VcsError::Invalid(
            "an integration commit needs at least one parent".to_string(),
        ));
    }

    // Pin every parent to an oid before doing anything else. Branch tips move: an agent
    // can commit between the prediction and the commit-tree call, and an integration
    // commit whose parents are not the commits that were predicted is exactly the silent
    // corruption this crate exists to prevent.
    let pinned: Vec<String> =
        parents.iter().map(|p| git::resolve_commit(repo, p)).collect::<Result<_>>()?;

    if pinned.len() == 1 {
        // A single dependency needs no merge, but it still gets a commit: the caller's
        // contract is "hand me one commit that contains all of these", and returning the
        // dependency's own tip would make the integration invisible in history.
        let tree = tree_of(repo, &pinned[0])?;
        return commit_tree(repo, &tree, &pinned, message, identity);
    }

    let total = pinned.len() - 1;
    let mut acc = pinned[0].clone();
    for (step, parent) in pinned[1..].iter().enumerate() {
        let prediction = predict_merge(repo, &acc, parent)?;
        if !prediction.clean {
            return Err(VcsError::MergeConflict {
                a: acc,
                b: parent.clone(),
                prediction: Box::new(prediction),
            });
        }
        let step_message = if total == 1 {
            message.to_string()
        } else {
            // Intermediate commits stay in history, so they say what they are; a chain of
            // identical messages is unreadable when an integration later needs debugging.
            format!("{message}\n\nwkbd: pairwise integration step {}/{}", step + 1, total)
        };
        acc = commit_tree(repo, &prediction.tree_oid, &[acc.clone(), parent.clone()], &step_message, identity)?;
    }
    Ok(acc)
}

fn tree_of(repo: &Path, commit: &str) -> Result<String> {
    let spec = format!("{commit}^{{tree}}");
    let out = git::run_checked(repo, &["rev-parse", "--verify", &spec])?;
    Ok(out.stdout_trimmed()?.to_string())
}

fn commit_tree(
    repo: &Path,
    tree: &str,
    parents: &[String],
    message: &str,
    identity: &Identity,
) -> Result<String> {
    let mut args: Vec<String> = vec!["commit-tree".to_string(), tree.to_string()];
    for p in parents {
        args.push("-p".to_string());
        args.push(p.clone());
    }
    args.push("-m".to_string());
    args.push(message.to_string());

    let env = [
        ("GIT_AUTHOR_NAME", identity.name.as_str()),
        ("GIT_AUTHOR_EMAIL", identity.email.as_str()),
        ("GIT_COMMITTER_NAME", identity.name.as_str()),
        ("GIT_COMMITTER_EMAIL", identity.email.as_str()),
    ];
    let out = git::run_with_env(repo, &args, &env)?;
    if !out.success() {
        return Err(out.error());
    }
    let oid = out.stdout_trimmed()?.to_string();
    if !git::looks_like_oid(&oid) {
        return Err(VcsError::Parse {
            invocation: out.invocation.clone(),
            detail: format!("commit-tree printed `{oid}`, which is not an object id"),
        });
    }
    Ok(oid)
}

fn detail_or_stderr(stderr: &str, parse_detail: &str) -> String {
    let condensed = git::first_lines(stderr, 3);
    if condensed.is_empty() {
        parse_detail.to_string()
    } else {
        condensed
    }
}

/// Parses the `--write-tree` output. Measured shape:
///
/// ```text
/// <tree oid>\n                                   clean: this is the entire output
/// <mode> <object> <stage>\t<path>\n              zero or more (may be zero on conflict)
/// \n                                             present only when a conflict occurred
/// <informational message>\n                      free-form, one per line
/// ```
///
/// Returns `Err(detail)` rather than a `VcsError` so the caller can decide whether an
/// unparsable body means "git failed and told us why on stderr" or "the format we pin has
/// changed", which are very different reports.
fn parse_write_tree(text: &str) -> std::result::Result<MergePrediction, String> {
    let mut lines = text.split('\n');
    let tree_oid = lines.next().unwrap_or("").trim_end_matches('\r').to_string();
    if !git::looks_like_oid(&tree_oid) {
        return Err(format!("expected a tree oid on the first line, got `{}`", truncate(&tree_oid, 120)));
    }

    let mut conflicted_paths = Vec::new();
    let mut messages = Vec::new();
    let mut in_messages = false;
    for raw in lines {
        let line = raw.trim_end_matches('\r');
        if in_messages {
            if !line.is_empty() {
                messages.push(line.to_string());
            }
            continue;
        }
        if line.is_empty() {
            // The blank line ends the conflicted-file section. Everything after it is
            // human-readable text and must not be parsed as data.
            in_messages = true;
            continue;
        }
        conflicted_paths.push(parse_conflicted_line(line)?);
    }

    Ok(MergePrediction { clean: false, tree_oid, conflicted_paths, messages })
}

fn parse_conflicted_line(line: &str) -> std::result::Result<ConflictedPath, String> {
    let (head, name) = line
        .split_once('\t')
        .ok_or_else(|| format!("conflicted file entry has no tab: `{}`", truncate(line, 120)))?;
    let fields: Vec<&str> = head.split(' ').collect();
    if fields.len() != 3 {
        return Err(format!("expected `<mode> <object> <stage>`, got `{}`", truncate(head, 120)));
    }
    let stage: u8 = fields[2]
        .parse()
        .map_err(|_| format!("stage `{}` is not a number", truncate(fields[2], 20)))?;
    if !(1..=3).contains(&stage) {
        return Err(format!("stage {stage} is outside git's 1..=3"));
    }
    let path = git::unquote_c_style(name).map_err(|e| e.to_string())?;
    Ok(ConflictedPath {
        mode: fields[0].to_string(),
        oid: fields[1].to_string(),
        stage,
        path,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_output() {
        let p = parse_write_tree("04ad535e67bb516d3db98802cd898c41a5319338\n").unwrap();
        assert_eq!(p.tree_oid, "04ad535e67bb516d3db98802cd898c41a5319338");
        assert!(p.conflicted_paths.is_empty());
        assert!(p.messages.is_empty());
    }

    #[test]
    fn parses_conflict_output() {
        let text = "e428495f17383d4a9e5d1c56de14b151740220be\n\
                    100644 3582182a1f64c9f6ec59ce981bfe0a59e85f1301 1\ta.txt\n\
                    100644 ca1a4b10e8727da5cfb6a7fb4774c72a92b5ec06 2\ta.txt\n\
                    100644 bd09a7112a0a85ec8987407994070b5e2558bfdb 3\ta.txt\n\
                    100644 4ae8ef021bf6fcfff43a13be5abfa52bb6fb5dbc 1\t\"unic\\303\\266de.txt\"\n\
                    \n\
                    Auto-merging a.txt\n\
                    CONFLICT (content): Merge conflict in a.txt\n";
        let p = parse_write_tree(text).unwrap();
        assert_eq!(p.conflicted_paths.len(), 4);
        assert_eq!(p.conflicted_paths[0].stage, 1);
        assert_eq!(p.conflicted_paths[3].path, "unicöde.txt");
        assert_eq!(p.conflicted_files(), vec!["a.txt", "unicöde.txt"]);
        assert_eq!(p.messages.len(), 2);
    }

    #[test]
    fn parses_conflict_with_no_conflicted_files() {
        // The directory-rename-split shape: a conflict nobody can point at a file for.
        let text = "2f3f9f389c27f6bcf97898812bb6daa365927ce2\n\n\
                    CONFLICT (directory rename split): Unclear where to rename dirA to\n";
        let p = parse_write_tree(text).unwrap();
        assert!(p.conflicted_paths.is_empty());
        assert_eq!(p.messages.len(), 1);
    }

    #[test]
    fn rejects_output_without_a_tree() {
        // What `merge-tree` produces for a bad ref: exit 1 and nothing on stdout.
        assert!(parse_write_tree("").is_err());
        assert!(parse_write_tree("fatal: not a commit\n").is_err());
    }
}
