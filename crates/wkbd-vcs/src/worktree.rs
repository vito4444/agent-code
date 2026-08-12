//! Worktree lifecycle, plus the part everyone forgets: making a fresh worktree usable.
//!
//! A worktree gives each agent its own checkout of its own branch. What it does **not**
//! give is isolation, and the design here follows from four things measured on this
//! machine (docs/M0-FINDINGS.md §2):
//!
//! - `git worktree add` runs `.git/hooks/post-checkout`. Every git call in this crate
//!   passes `-c core.hooksPath=/dev/null`; see `crate::git`.
//! - `.git/config` is shared by all worktrees, so one agent writing `core.pager` executes
//!   code in every other agent's next paginated command. Nothing here writes config, and
//!   confinement of the agents themselves is `wkbd-sec`'s job.
//! - `refs/stash` is shared, so `git stash` in one worktree is visible — and poppable —
//!   in all of them.
//! - git refuses to check out a branch that another worktree already has. That refusal is
//!   useful: it is git enforcing "never rewrite a branch someone else is working in" on
//!   our behalf, so [`add_worktree`] surfaces it as its own error rather than flattening
//!   it into a generic git failure.
//!
//! The other half of this module is [`hydrate`]. A newly created worktree is *inert*: it
//! has every tracked file and nothing else. No `.env`, no `node_modules`, no local
//! database, no built artifacts — all of them gitignored, all of them required to run
//! anything. An agent dropped into an inert worktree spends its turn discovering that the
//! project does not build, and reports failure for a task that was fine. Hydration is the
//! step that fixes that, and it is deliberately explicit: copying whatever is untracked
//! would copy the user's credentials into every sandbox.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VcsError};
use crate::git;
use crate::pathset::{self, Pattern};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The repository the worktree belongs to (any path inside it works for git calls).
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
}

/// One record of `git worktree list --porcelain`. Fields mirror the porcelain labels
/// exactly; anything git may add later is ignored rather than guessed at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Full ref name as git prints it, e.g. `refs/heads/feature`.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prune_reason: Option<String>,
}

impl WorktreeInfo {
    /// Short branch name, so callers can compare against what a user or a model typed.
    pub fn branch_name(&self) -> Option<&str> {
        self.branch.as_deref().map(short_branch)
    }
}

fn short_branch(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// Creates `branch` at `start_commit` and checks it out into a new worktree at `path`.
///
/// The branch is always created, never reused: adopting an existing branch would mean
/// pointing an agent at work someone else may still own, and moving it to `start_commit`
/// would discard that work outright.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, start_commit: &str) -> Result<Worktree> {
    // `git worktree list --porcelain` is newline-delimited and does not quote paths, so a
    // worktree whose path contains a newline cannot be read back reliably. Refusing to
    // create one keeps the parser honest instead of making it guess.
    let path_str = path.to_string_lossy();
    if path_str.contains('\n') {
        return Err(VcsError::UnsafePath {
            path: path_str.into_owned(),
            reason: "worktree paths must not contain newlines",
        });
    }

    // Checked before asking git purely for the error message: git's own refusal names the
    // conflicting worktree, but only in prose on stderr. This is still racy by nature, so
    // git's refusal is mapped below as well.
    if let Some(existing) = is_branch_checked_out(repo, branch)? {
        return Err(VcsError::BranchAlreadyCheckedOut { branch: branch.to_string(), path: existing });
    }

    let start = git::resolve_commit(repo, start_commit)?;
    let args: Vec<&OsStr> = vec![
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("-b"),
        OsStr::new(branch),
        path.as_os_str(),
        OsStr::new(&start),
    ];
    let out = git::run(repo, &args)?;
    if !out.success() {
        return Err(classify_add_failure(&out.stderr, branch, out.error()));
    }

    Ok(Worktree {
        repo: repo.to_path_buf(),
        path: path.to_path_buf(),
        branch: branch.to_string(),
        head: start,
    })
}

fn classify_add_failure(stderr: &str, branch: &str, fallback: VcsError) -> VcsError {
    if let Some(rest) = stderr.split("is already used by worktree at").nth(1) {
        let existing = rest.trim().trim_matches(['\'', '"', '\n']).to_string();
        return VcsError::BranchAlreadyCheckedOut { branch: branch.to_string(), path: PathBuf::from(existing) };
    }
    if stderr.contains("already exists") {
        return VcsError::BranchExists { branch: branch.to_string() };
    }
    fallback
}

/// Parses `git worktree list --porcelain`.
///
/// The porcelain format is the only stable interface here; the default human-readable
/// output aligns columns, elides the branch for detached heads and has no way to express
/// a lock reason, so parsing it would be guesswork that changes between git versions.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let out = git::run_checked(repo, &["worktree", "list", "--porcelain"])?;
    parse_porcelain(out.stdout_utf8()?)
}

fn parse_porcelain(text: &str) -> Result<Vec<WorktreeInfo>> {
    let mut all = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            // Records are separated by a blank line. A bare repository's record has no
            // HEAD line at all, so "record ended" cannot be inferred from the fields.
            if let Some(info) = current.take() {
                all.push(info);
            }
            continue;
        }
        let (label, value) = match line.split_once(' ') {
            Some((l, v)) => (l, Some(v)),
            None => (line, None),
        };
        match label {
            "worktree" => {
                if let Some(info) = current.take() {
                    all.push(info);
                }
                let path = value.ok_or_else(|| VcsError::Parse {
                    invocation: "worktree list --porcelain".to_string(),
                    detail: "`worktree` line without a path".to_string(),
                })?;
                current = Some(WorktreeInfo { path: PathBuf::from(path), ..Default::default() });
            }
            _ => {
                let info = current.as_mut().ok_or_else(|| VcsError::Parse {
                    invocation: "worktree list --porcelain".to_string(),
                    detail: format!("`{label}` appeared before any `worktree` line"),
                })?;
                match label {
                    "HEAD" => info.head = value.map(str::to_string),
                    "branch" => info.branch = value.map(str::to_string),
                    "bare" => info.bare = true,
                    "detached" => info.detached = true,
                    "locked" => {
                        info.locked = true;
                        info.lock_reason = value.map(str::to_string);
                    }
                    "prunable" => {
                        info.prunable = true;
                        info.prune_reason = value.map(str::to_string);
                    }
                    // Unknown labels are ignored on purpose: git adds attributes over
                    // time, and refusing to list worktrees because of one unrecognised
                    // line would take the scheduler down for a cosmetic reason.
                    other => tracing::debug!(label = other, "ignoring unknown worktree attribute"),
                }
            }
        }
    }
    if let Some(info) = current.take() {
        all.push(info);
    }
    Ok(all)
}

/// Where `branch` is checked out, if anywhere.
///
/// The scheduler calls this before any operation that would move a branch. git enforces
/// the same rule at checkout time, but only for checkout: `git branch -f`, `git reset`
/// and friends will happily rewrite a branch under a running agent.
pub fn is_branch_checked_out(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let wanted = short_branch(branch);
    Ok(list_worktrees(repo)?
        .into_iter()
        .find(|w| w.branch_name() == Some(wanted))
        .map(|w| w.path))
}

/// Removes a worktree. Without `force`, git refuses when the worktree contains modified
/// or untracked files — which is the desired default: an agent's uncommitted work is the
/// most expensive thing in the system to recreate.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let mut args: Vec<&OsStr> = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if force {
        args.push(OsStr::new("--force"));
    }
    args.push(path.as_os_str());

    let out = git::run(repo, &args)?;
    if out.success() {
        return Ok(());
    }
    if out.stderr.contains("contains modified or untracked files") {
        return Err(VcsError::WorktreeNotClean { path: path.to_path_buf() });
    }
    Err(out.error())
}

// ---------------------------------------------------------------------------- hydration

/// What a fresh worktree needs on top of its tracked files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HydrationSpec {
    /// The working tree to copy from — normally the user's main checkout, which is the
    /// only place the untracked-but-required files exist.
    pub source: PathBuf,
    /// Relative globs, matched against the source tree. Explicit by design: an
    /// "everything untracked" mode would copy `.ssh` shaped mistakes, private keys and
    /// multi-gigabyte build caches into every agent sandbox.
    pub copy_globs: Vec<String>,
    /// Commands run in the new worktree afterwards (`npm ci`, `cargo fetch`, a migration).
    pub setup_commands: Vec<SetupCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupCommand {
    pub program: String,
    pub args: Vec<String>,
    /// A setup command that hangs holds an agent slot forever, and package managers hang
    /// for perfectly ordinary reasons (a prompt, a dead registry). `None` means wait
    /// indefinitely and should be reserved for commands known to terminate.
    pub timeout: Option<Duration>,
}

impl SetupCommand {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        SetupCommand {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: Some(Duration::from_secs(600)),
        }
    }

    fn display(&self) -> String {
        std::iter::once(self.program.clone()).chain(self.args.iter().cloned()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// Never followed. A symlink in the source can point anywhere the daemon can read
    /// (`/etc/shadow`, the user's `~/.aws/credentials`), and copying through it would
    /// materialise that content as a real file inside a directory an agent can read.
    Symlink,
    /// The resolved destination left the worktree, or would be written through an
    /// existing symlink inside it.
    EscapesRoot,
    /// Sockets, fifos, devices: nothing a project needs, everything a copy can hang on.
    NotRegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPath {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HydrationReport {
    pub copied: Vec<String>,
    /// Surfaced rather than silently dropped: "the agent could not run the tests" and
    /// "we refused to copy the symlink that pointed at the test fixtures" look identical
    /// from inside the sandbox.
    pub skipped: Vec<SkippedPath>,
    pub commands: Vec<CommandOutcome>,
}

/// Copies the declared files into `worktree` and runs the setup commands there.
///
/// Path safety is enforced here rather than delegated. `wkbd-sec::path_guard` confines
/// paths for a different threat model — an agent naming a path over the protocol, where
/// the answer must be an already-open descriptor so it cannot go stale — and this crate
/// does not depend on it: hydration is a daemon-to-daemon copy between two trees we
/// chose, and taking a dependency on an interface that is still being written would make
/// the one operation that reads the user's real checkout the first casualty of an API
/// change. The rules enforced here are: no symlink is followed or copied in either tree,
/// patterns may not be absolute or contain `..` (see `pathset`), and every destination is
/// re-checked to be inside the worktree after resolution. If these ever need to become
/// three rules instead of one implementation, they should move behind `wkbd-sec` rather
/// than be copied a second time.
pub fn hydrate(worktree: &Path, spec: &HydrationSpec) -> Result<HydrationReport> {
    let mut report = HydrationReport::default();

    let dest_root = worktree
        .canonicalize()
        .map_err(|e| VcsError::io("canonicalize worktree", worktree, e))?;

    if !spec.copy_globs.is_empty() {
        let source_root = spec
            .source
            .canonicalize()
            .map_err(|e| VcsError::io("canonicalize hydration source", &spec.source, e))?;
        if source_root == dest_root {
            return Err(VcsError::Invalid(
                "hydration source and destination are the same directory".to_string(),
            ));
        }
        let patterns = pathset::parse_all(&spec.copy_globs)?;
        copy_tree(&source_root, &dest_root, &patterns, &mut report)?;
    }

    for command in &spec.setup_commands {
        report.commands.push(run_setup(&dest_root, command)?);
    }
    Ok(report)
}

fn copy_tree(
    source_root: &Path,
    dest_root: &Path,
    patterns: &[Pattern],
    report: &mut HydrationReport,
) -> Result<()> {
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = source_root.join(&rel_dir);
        let entries = std::fs::read_dir(&dir).map_err(|e| VcsError::io("read directory", &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| VcsError::io("read directory entry", &dir, e))?;
            let name = entry.file_name();
            let rel = rel_dir.join(&name);
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            // `.git` is the repository itself, not project state. Copying it into a
            // worktree produces a checkout that git will read in preference to the real
            // repository, which is a spectacular way to lose work.
            if name == OsStr::new(".git") {
                continue;
            }

            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| VcsError::io("stat", entry.path(), e))?;

            if meta.file_type().is_symlink() {
                if pattern_could_reach(patterns, &rel_str) {
                    report.skipped.push(SkippedPath { path: rel_str, reason: SkipReason::Symlink });
                }
                continue;
            }
            if meta.is_dir() {
                stack.push(rel);
                continue;
            }
            if !meta.is_file() {
                if pathset::any_match(patterns, &rel_str) {
                    report.skipped.push(SkippedPath { path: rel_str, reason: SkipReason::NotRegularFile });
                }
                continue;
            }
            if !pathset::any_match(patterns, &rel_str) {
                continue;
            }

            match copy_one(source_root, dest_root, &rel) {
                Ok(()) => report.copied.push(rel_str),
                Err(VcsError::UnsafePath { .. }) => {
                    report.skipped.push(SkippedPath { path: rel_str, reason: SkipReason::EscapesRoot })
                }
                Err(other) => return Err(other),
            }
        }
    }
    report.copied.sort();
    report.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// A symlink is reported as skipped when a pattern names it or could match something
/// beneath it, so that `logs/**` pointing at a symlinked `logs` shows up in the report
/// instead of looking like an empty directory.
fn pattern_could_reach(patterns: &[Pattern], rel: &str) -> bool {
    let prefix = format!("{rel}/");
    patterns.iter().any(|p| p.is_match(rel) || p.source.starts_with(&prefix))
}

fn copy_one(source_root: &Path, dest_root: &Path, rel: &Path) -> Result<()> {
    for component in rel.components() {
        // The walk only produces normal components, so this is a guard against a future
        // caller building `rel` some other way rather than against the filesystem.
        if !matches!(component, Component::Normal(_)) {
            return Err(VcsError::UnsafePath {
                path: rel.to_string_lossy().into_owned(),
                reason: "relative path component is not a plain name",
            });
        }
    }

    let dest = dest_root.join(rel);
    // Walk the destination path from the root down. An existing symlinked directory
    // inside the worktree would otherwise let a copy land outside it — the agent controls
    // this tree, so it can plant one between hydration runs.
    let mut probe = dest_root.to_path_buf();
    for component in rel.components() {
        probe.push(component);
        match probe.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(VcsError::UnsafePath {
                    path: probe.to_string_lossy().into_owned(),
                    reason: "destination path crosses a symlink",
                })
            }
            _ => {}
        }
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| VcsError::io("create directory", parent, e))?;
        let resolved = parent.canonicalize().map_err(|e| VcsError::io("canonicalize", parent, e))?;
        if !resolved.starts_with(dest_root) {
            return Err(VcsError::UnsafePath {
                path: resolved.to_string_lossy().into_owned(),
                reason: "destination resolves outside the worktree",
            });
        }
    }

    let src = source_root.join(rel);
    std::fs::copy(&src, &dest).map_err(|e| VcsError::io("copy file", &src, e))?;
    Ok(())
}

fn run_setup(cwd: &Path, command: &SetupCommand) -> Result<CommandOutcome> {
    let display = command.display();
    let started = Instant::now();

    // No shell. Setup commands come from project configuration that a model may have
    // written, and `sh -c` would turn a string in a config file into arbitrary code with
    // no argument boundaries.
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| VcsError::SetupCommand {
            command: display.clone(),
            detail: format!("could not start: {source}"),
        })?;

    let status = match command.timeout {
        None => child.wait().map_err(|e| VcsError::SetupCommand {
            command: display.clone(),
            detail: format!("could not wait for the process: {e}"),
        })?,
        Some(limit) => loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= limit => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(VcsError::SetupCommand {
                        command: display,
                        detail: format!("timed out after {limit:?} and was killed"),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => {
                    return Err(VcsError::SetupCommand {
                        command: display,
                        detail: format!("could not poll the process: {e}"),
                    })
                }
            }
        },
    };

    let stdout = read_pipe(child.stdout.take());
    let stderr = read_pipe(child.stderr.take());
    let outcome = CommandOutcome {
        command: display.clone(),
        exit_code: status.code(),
        stdout,
        stderr,
        duration: started.elapsed(),
    };

    if !status.success() {
        // Fail loudly. A half-hydrated worktree makes the agent fail at something
        // unrelated later, and that failure gets attributed to the model.
        return Err(VcsError::SetupCommand {
            command: display,
            detail: format!(
                "exited with {}: {}",
                outcome.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".to_string()),
                git::first_lines(&outcome.stderr, 3)
            ),
        });
    }
    Ok(outcome)
}

fn read_pipe(pipe: Option<impl Read>) -> String {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_porcelain_attribute() {
        let text = "worktree /r/main\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /r/det\nHEAD def\ndetached\n\n\
                    worktree /r/locked\nHEAD 123\nbranch refs/heads/x\nlocked busy testing\n\n\
                    worktree /r/gone\nHEAD 456\ndetached\nprunable gitdir file points to non-existent location\n\n\
                    worktree /r/bare.git\nbare\n\n";
        let parsed = parse_porcelain(text).unwrap();
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0].branch_name(), Some("main"));
        assert!(parsed[1].detached && parsed[1].branch.is_none());
        assert_eq!(parsed[2].lock_reason.as_deref(), Some("busy testing"));
        assert!(parsed[3].prunable);
        assert!(parsed[4].bare && parsed[4].head.is_none());
    }

    #[test]
    fn parses_a_record_without_a_trailing_blank_line() {
        let parsed = parse_porcelain("worktree /r/main\nHEAD abc\nbranch refs/heads/main\n").unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn keeps_spaces_in_paths() {
        let parsed = parse_porcelain("worktree /tmp/wt feat\nHEAD abc\ndetached\n").unwrap();
        assert_eq!(parsed[0].path, PathBuf::from("/tmp/wt feat"));
    }
}

/// Commits everything an agent changed in its worktree, and reports the commit.
///
/// The merge machinery works on commits, not on working trees: `merge-tree` reads objects, and a
/// dirty worktree has nothing for it to read. So a task's result has to become a commit before it
/// can be combined with anything, and this is where that happens.
///
/// Returns `None` when there was nothing to commit. That is not an error — an agent can legitimately
/// decide a task needs no change — but it is also indistinguishable from an agent that did nothing,
/// which is why it is reported rather than turned into an empty commit that hides the difference.
///
/// The author identity is supplied rather than inherited. A daemon can easily be running somewhere
/// `user.email` was never configured, and there git refuses to commit at all: the run would fail at
/// the last step, after all the work, for a reason that has nothing to do with the work.
pub fn commit_all(
    worktree: &Path,
    message: &str,
    identity: &crate::merge::Identity,
) -> Result<Option<String>> {
    // `add -A` rather than `add .`: the latter misses deletions in some git versions, and a task
    // that deletes a file would silently produce a commit that still contains it.
    let add = git::run(worktree, &["add", "-A"])?;
    if !add.success() {
        return Err(add.error());
    }

    let staged = git::run(worktree, &["diff", "--cached", "--name-only", "-z"])?;
    if !staged.success() {
        return Err(staged.error());
    }
    if staged.nul_fields()?.is_empty() {
        return Ok(None);
    }

    let out = git::run_with_env(
        worktree,
        &["commit", "--no-verify", "--no-gpg-sign", "-m", message],
        &[
            ("GIT_AUTHOR_NAME", identity.name.as_str()),
            ("GIT_AUTHOR_EMAIL", identity.email.as_str()),
            ("GIT_COMMITTER_NAME", identity.name.as_str()),
            ("GIT_COMMITTER_EMAIL", identity.email.as_str()),
        ],
    )?;
    if !out.success() {
        return Err(out.error());
    }

    let head = git::run(worktree, &["rev-parse", "HEAD"])?;
    if !head.success() {
        return Err(head.error());
    }
    Ok(Some(head.stdout_trimmed()?.to_string()))
}
