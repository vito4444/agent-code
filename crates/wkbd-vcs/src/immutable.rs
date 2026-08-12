//! Keeping the acceptance criteria out of the agent's reach.
//!
//! Automated acceptance is worth nothing if the thing being accepted can edit the test
//! suite. This is not hypothetical: an audit of SWE-bench Verified found 28.5% of sampled
//! tasks had test suites weak enough to pass an incorrect patch, and a roughly ten-line
//! `conftest.py` — executed automatically during pytest's collection phase, before any
//! test runs — is enough to hook the reporting hook and mark every test passed. A team
//! that did exactly that scored 100% with a patch that fixed nothing.
//!
//! The shape that closes this is five steps, and the important detail is which of them is
//! the actual defence:
//!
//! | step   | here                                              |
//! |--------|---------------------------------------------------|
//! | init   | [`snapshot_paths_with_contents`] — hash + content  |
//! | lock   | [`lock_paths`] — make them read-only              |
//! | run    | [`restore_from_snapshot`] **then** run the tests   |
//! | verify | [`verify_snapshot`] — report what moved            |
//! | accept | caller's decision                                  |
//!
//! `lock` is a speed bump; anything that can `chmod` can undo it, and the harness usually
//! runs as the same user as the agent. `restore` is the defence: putting the originals
//! back immediately before the tests run makes every edit the agent made irrelevant
//! regardless of how it made it. `verify` exists so tampering is reported rather than
//! silently absorbed, because "the agent tried to rewrite the tests" is the single most
//! useful thing this system can tell a user about a run.
//!
//! Restoration also deletes files that appeared under the watched patterns but were not
//! in the snapshot. That is not tidiness: `conftest.py` is an *added* file, so a restore
//! that only rewrites known files leaves the entire attack in place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Result, VcsError};
use crate::git;
use crate::pathset::{self, Pattern};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Relative to the snapshot root, `/`-separated.
    pub path: String,
    pub sha256: String,
    /// Unix mode bits, so a restored file comes back with the permissions it had rather
    /// than the ones locking left behind.
    pub mode: u32,
    /// Where the content was copied to, when the caller asked for content storage.
    pub stored: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: PathBuf,
    pub globs: Vec<String>,
    pub entries: Vec<SnapshotEntry>,
    pub content_dir: Option<PathBuf>,
}

impl Snapshot {
    pub fn find(&self, relative_path: &str) -> Option<&SnapshotEntry> {
        self.entries.iter().find(|e| e.path == relative_path)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Present, but the content hash changed.
    pub modified: Vec<String>,
    /// In the snapshot, gone from disk.
    pub missing: Vec<String>,
    /// Matches the watched patterns, was not in the snapshot. The `conftest.py` case.
    pub added: Vec<String>,
    /// Replaced by a symlink. Called out separately because the content behind it can
    /// hash correctly while pointing somewhere the harness never intended to read.
    pub replaced_by_symlink: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.modified.is_empty()
            && self.missing.is_empty()
            && self.added.is_empty()
            && self.replaced_by_symlink.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

/// Hashes every file matching `globs` under `root`. Content is not kept, so the result
/// can detect tampering but cannot undo it — use [`snapshot_paths_with_contents`] for the
/// snapshot the harness restores from.
pub fn snapshot_paths(root: &Path, globs: &[String]) -> Result<Snapshot> {
    snapshot_inner(root, globs, None)
}

/// As [`snapshot_paths`], but each file's bytes are copied into `content_dir`.
///
/// The caller supplies the directory because its lifetime is the caller's problem and its
/// location matters: it must be somewhere the agent cannot write, or the restore step
/// restores whatever the agent put there.
pub fn snapshot_paths_with_contents(root: &Path, globs: &[String], content_dir: &Path) -> Result<Snapshot> {
    std::fs::create_dir_all(content_dir)
        .map_err(|e| VcsError::io("create snapshot content directory", content_dir, e))?;
    snapshot_inner(root, globs, Some(content_dir))
}

fn snapshot_inner(root: &Path, globs: &[String], content_dir: Option<&Path>) -> Result<Snapshot> {
    let root = canonical_root(root)?;
    let patterns = pathset::parse_all(globs)?;
    let mut entries = Vec::new();

    for rel in walk_matching(&root, &patterns)? {
        let absolute = root.join(&rel);
        let bytes = std::fs::read(&absolute).map_err(|e| VcsError::io("read file", &absolute, e))?;
        let digest = hex::encode(Sha256::digest(&bytes));
        let mode = mode_of(&absolute)?;

        let stored = match content_dir {
            None => None,
            Some(dir) => {
                // Named by digest: identical files are stored once, and a stored file
                // cannot be confused with a different version of the same path.
                let target = dir.join(&digest);
                if !target.exists() {
                    std::fs::write(&target, &bytes)
                        .map_err(|e| VcsError::io("write snapshot content", &target, e))?;
                }
                Some(target)
            }
        };
        entries.push(SnapshotEntry { path: rel, sha256: digest, mode, stored });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Snapshot {
        root,
        globs: globs.to_vec(),
        entries,
        content_dir: content_dir.map(Path::to_path_buf),
    })
}

/// Makes the matching files read-only by clearing their write bits.
///
/// Worth doing because it stops the accidental case and makes the deliberate case
/// deliberate — an agent that chmods the test suite before editing it has done something
/// no honest workflow does, and [`verify_snapshot`] will say so. It is not a boundary:
/// the directories stay writable (they have to; the build writes there), so a file can
/// still be deleted and recreated. Running as root defeats it entirely.
///
/// Clearing bits rather than assigning `0o444`, and this is not a detail. Assigning a mode drops
/// the executable bit, and the file most likely to be protected is the one that runs the tests. The
/// effects compound: git tracks the executable bit, so the chmod appears in `git diff` as a
/// modification to a file the task never touched — which the ownership check then reports as the
/// task changing something it did not declare. So protecting the acceptance suite would fail every
/// task, and the reported reason would name the wrong culprit.
pub fn lock_paths(root: &Path, globs: &[String]) -> Result<Vec<PathBuf>> {
    adjust_mode(root, globs, |mode| mode & !0o222)
}

/// Restores write permission for the owner, for the harness's own updates and for cleanup.
///
/// Adds one bit rather than assigning a mode, for the same reason: the previous implementation
/// assigned `0o644` and permanently unset the executable bit on whatever it had protected.
pub fn unlock_paths(root: &Path, globs: &[String]) -> Result<Vec<PathBuf>> {
    adjust_mode(root, globs, |mode| mode | 0o200)
}

fn adjust_mode(
    root: &Path,
    globs: &[String],
    f: impl Fn(u32) -> u32,
) -> Result<Vec<PathBuf>> {
    let root = canonical_root(root)?;
    let patterns = pathset::parse_all(globs)?;
    let mut touched = Vec::new();
    for rel in walk_matching(&root, &patterns)? {
        let absolute = root.join(&rel);
        let current = std::fs::metadata(&absolute)
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode()
            })
            .map_err(|source| VcsError::Io {
                operation: "reading a mode before changing it",
                path: absolute.clone(),
                source,
            })?;
        set_file_mode(&absolute, f(current & 0o7777))?;
        touched.push(absolute);
    }
    Ok(touched)
}

/// Puts the snapshotted files back exactly as they were, and deletes anything that
/// appeared under the watched patterns since.
///
/// Call this immediately before running the acceptance suite, every time. It is what
/// makes the file permissions above unnecessary rather than load-bearing.
pub fn restore_from_snapshot(root: &Path, snapshot: &Snapshot) -> Result<RestoreReport> {
    let root = canonical_root(root)?;
    let content_dir_missing = snapshot.entries.iter().any(|e| e.stored.is_none());
    if content_dir_missing {
        return Err(VcsError::Invalid(
            "this snapshot stores hashes only; take it with snapshot_paths_with_contents to restore"
                .to_string(),
        ));
    }

    let mut report = RestoreReport::default();
    let mut expected: BTreeMap<&str, &SnapshotEntry> = BTreeMap::new();
    for entry in &snapshot.entries {
        expected.insert(entry.path.as_str(), entry);
    }

    for entry in &snapshot.entries {
        let target = root.join(&entry.path);
        let stored = entry.stored.as_ref().expect("checked above");
        let wanted = std::fs::read(stored).map_err(|e| VcsError::io("read snapshot content", stored, e))?;

        let identical = match target.symlink_metadata() {
            // A symlink is never left in place even if it resolves to the right bytes:
            // the next write through it lands wherever it points.
            Ok(meta) if meta.file_type().is_symlink() => {
                std::fs::remove_file(&target).map_err(|e| VcsError::io("remove symlink", &target, e))?;
                false
            }
            Ok(_) => std::fs::read(&target).map(|current| current == wanted).unwrap_or(false),
            Err(_) => false,
        };

        if identical && mode_of(&target)? == entry.mode {
            report.unchanged.push(entry.path.clone());
            continue;
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| VcsError::io("create directory", parent, e))?;
        }
        // The file is probably 0444 from `lock_paths`, so make it writable first rather
        // than reporting a permission error caused by our own lock.
        if target.exists() {
            set_file_mode(&target, 0o644)?;
        }
        std::fs::write(&target, &wanted).map_err(|e| VcsError::io("restore file", &target, e))?;
        set_file_mode(&target, entry.mode)?;
        report.restored.push(entry.path.clone());
    }

    let patterns = pathset::parse_all(&snapshot.globs)?;
    for rel in walk_matching(&root, &patterns)? {
        if expected.contains_key(rel.as_str()) {
            continue;
        }
        let target = root.join(&rel);
        set_file_mode(&target, 0o644).ok();
        std::fs::remove_file(&target).map_err(|e| VcsError::io("remove added file", &target, e))?;
        report.removed.push(rel);
    }

    report.restored.sort();
    report.removed.sort();
    report.unchanged.sort();
    Ok(report)
}

/// Reports every difference between disk and the snapshot.
pub fn verify_snapshot(root: &Path, snapshot: &Snapshot) -> Result<VerifyReport> {
    let root = canonical_root(root)?;
    let mut report = VerifyReport::default();
    let mut known: BTreeMap<&str, &SnapshotEntry> = BTreeMap::new();
    for entry in &snapshot.entries {
        known.insert(entry.path.as_str(), entry);
    }

    for entry in &snapshot.entries {
        let target = root.join(&entry.path);
        match target.symlink_metadata() {
            Err(_) => report.missing.push(entry.path.clone()),
            Ok(meta) if meta.file_type().is_symlink() => {
                report.replaced_by_symlink.push(entry.path.clone())
            }
            Ok(_) => {
                let bytes = std::fs::read(&target).map_err(|e| VcsError::io("read file", &target, e))?;
                if hex::encode(Sha256::digest(&bytes)) != entry.sha256 {
                    report.modified.push(entry.path.clone());
                }
            }
        }
    }

    for rel in walk_matching(&root, &pathset::parse_all(&snapshot.globs)?)? {
        if !known.contains_key(rel.as_str()) {
            report.added.push(rel);
        }
    }

    report.modified.sort();
    report.missing.sort();
    report.added.sort();
    report.replaced_by_symlink.sort();
    Ok(report)
}

/// Restores files from a commit instead of from a content snapshot.
///
/// **The command is always `git checkout <commit> -- <explicit file list>`.** Never
/// `git checkout <commit>` on its own: that form resets the entire working tree, which
/// throws away everything the environment setup did (installed dependencies, generated
/// config, applied migrations) along with the agent's edits. The SWE-bench harness shipped
/// exactly that bug, and the result was every patch failing for a reason that had nothing
/// to do with the patch.
///
/// Files that do not exist in `commit` are removed with a plain unlink rather than
/// checked out — `git checkout` fails the whole invocation on a pathspec that matches
/// nothing, so a single added file would otherwise abort the restore of everything else.
pub fn restore_from_commit(repo: &Path, commit: &str, paths: &[String]) -> Result<RestoreReport> {
    let top = git::toplevel(repo)?;
    let pinned = git::resolve_commit(&top, commit)?;
    let mut report = RestoreReport::default();

    let mut present: Vec<String> = Vec::new();
    for path in paths {
        let path = pathset::require_concrete(path)?;
        let spec = format!("{pinned}:{path}");
        let probe = git::run(&top, &["cat-file", "-e", &spec])?;
        if probe.success() {
            present.push(path);
        } else {
            let target = top.join(&path);
            match std::fs::remove_file(&target) {
                Ok(()) => report.removed.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(VcsError::io("remove file absent from commit", &target, e)),
            }
        }
    }

    // Chunked so a large watched set cannot hit the argument length limit; the limit is
    // in the tens of thousands, and a test suite with that many files is not exotic.
    for chunk in present.chunks(128) {
        let mut args: Vec<String> = vec!["checkout".to_string(), pinned.clone(), "--".to_string()];
        args.extend(chunk.iter().cloned());
        git::run_checked(&top, &args)?;
        report.restored.extend(chunk.iter().cloned());
    }

    report.restored.sort();
    report.removed.sort();
    Ok(report)
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize().map_err(|e| VcsError::io("canonicalize root", root, e))
}

/// Depth-first walk returning `/`-separated relative paths of regular files that match.
/// Symlinks are never followed: the watched set describes files in this tree, and a
/// symlink is a way to make it describe files somewhere else.
fn walk_matching(root: &Path, patterns: &[Pattern]) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = root.join(&rel_dir);
        let entries = std::fs::read_dir(&dir).map_err(|e| VcsError::io("read directory", &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| VcsError::io("read directory entry", &dir, e))?;
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let rel = rel_dir.join(&name);
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| VcsError::io("stat", entry.path(), e))?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(rel);
                continue;
            }
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if meta.is_file() && pathset::any_match(patterns, &rel_str) {
                found.push(rel_str);
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| VcsError::io("stat", path, e))?;
    Ok(meta.permissions().mode() & 0o7777)
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| VcsError::io("set permissions", path, e))
}

#[cfg(not(unix))]
fn mode_of(path: &Path) -> Result<u32> {
    let meta = std::fs::metadata(path).map_err(|e| VcsError::io("stat", path, e))?;
    Ok(if meta.permissions().readonly() { 0o444 } else { 0o644 })
}

#[cfg(not(unix))]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .map_err(|e| VcsError::io("stat", path, e))?
        .permissions();
    perms.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, perms).map_err(|e| VcsError::io("set permissions", path, e))
}

#[cfg(test)]
mod mode_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// The file most likely to be protected is the one that runs the tests, and the previous
    /// implementation assigned `0o444` and then `0o644`, so protecting it left it unable to run. The
    /// second-order effect was worse: git tracks the executable bit, so the chmod showed up in
    /// `git diff` and the ownership check reported the task as having modified a file it never
    /// touched — failing every task for a reason that named the wrong file.
    #[test]
    fn locking_an_executable_file_leaves_it_executable() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("check.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let globs = vec!["*.sh".to_string()];
        lock_paths(dir.path(), &globs).unwrap();
        let locked = mode_of(&script);
        assert_eq!(locked & 0o222, 0, "must not be writable, got {locked:o}");
        assert_ne!(locked & 0o111, 0, "must stay executable, got {locked:o}");

        unlock_paths(dir.path(), &globs).unwrap();
        assert_eq!(mode_of(&script), 0o755, "the original mode must come back");
    }

    #[test]
    fn locking_a_plain_file_does_not_make_it_executable() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data.txt");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        let globs = vec!["*.txt".to_string()];
        lock_paths(dir.path(), &globs).unwrap();
        assert_eq!(mode_of(&f) & 0o111, 0);
        unlock_paths(dir.path(), &globs).unwrap();
        assert_eq!(mode_of(&f), 0o644);
    }

    /// The reason the mode matters at all: git records the executable bit, so changing it is a
    /// change to the file as far as every diff-based check is concerned.
    #[test]
    fn a_lock_and_unlock_cycle_leaves_nothing_for_git_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
                .output()
                .unwrap()
        };
        run(&["init", "-q", "."]);
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        let script = repo.join("tests/check.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "i"]);

        let globs = vec!["tests/**".to_string()];
        lock_paths(repo, &globs).unwrap();
        unlock_paths(repo, &globs).unwrap();

        let out = run(&["diff", "--name-only"]);
        let reported = String::from_utf8_lossy(&out.stdout);
        assert!(
            reported.trim().is_empty(),
            "protecting a file must not look like changing it; git reported: {reported}"
        );
    }
}
