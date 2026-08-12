//! Real repositories in real temporary directories.
//!
//! Nothing here mocks git. The behaviours this crate is built on — an exit code that
//! disagrees with the output, a conflict with no conflicting file — are behaviours of the
//! git binary, so a fake git would only ever confirm what we already believed.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

pub struct TestRepo {
    /// Kept alive for the lifetime of the fixture; worktrees are created as siblings of
    /// `path` inside it so that dropping the fixture removes everything.
    pub dir: TempDir,
    pub path: PathBuf,
}

impl TestRepo {
    pub fn init() -> TestRepo {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("repo");
        std::fs::create_dir_all(&path).expect("create repo dir");
        let repo = TestRepo { dir, path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "test@wkbd.invalid"]);
        repo.git(&["config", "user.name", "wkbd test"]);
        // A developer machine with commit signing configured globally would otherwise
        // fail every fixture commit for reasons that have nothing to do with the test.
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.path, args)
    }

    pub fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf-8 git output").trim_end().to_string()
    }

    pub fn write(&self, relative: &str, contents: &str) {
        write_under(&self.path, relative, contents);
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn branch_from(&self, name: &str, start: &str) {
        self.git(&["checkout", "-q", start]);
        self.git(&["checkout", "-q", "-b", name]);
    }

    pub fn status(&self) -> String {
        self.git(&["status", "--porcelain"])
    }

    /// A path inside the fixture but outside the repository, for worktrees.
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

pub fn write_under(root: &Path, relative: &str, contents: &str) {
    let target = root.join(relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&target, contents).unwrap_or_else(|e| panic!("write {}: {e}", target.display()));
}

pub fn read_under(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"))
}

pub fn status_of(cwd: &Path) -> String {
    let out = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .expect("run git status");
    assert!(out.status.success(), "git status failed in {}", cwd.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn rev_parse(cwd: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "rev-parse", rev])
        .current_dir(cwd)
        .output()
        .expect("run git rev-parse");
    assert!(out.status.success(), "git rev-parse {rev} failed in {}", cwd.display());
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Builds the fixture from docs/M0-FINDINGS.md §1.3: branch `P` splits `dirA/` into two
/// directories, branch `Q` adds a file inside `dirA/`. The two branches touch no path in
/// common, and the merge still conflicts.
pub struct RenameSplit {
    pub repo: TestRepo,
    pub base: String,
    pub p: String,
    pub q: String,
}

pub fn directory_rename_split() -> RenameSplit {
    let repo = TestRepo::init();
    repo.write("dirA/f1.txt", "c1\n");
    repo.write("dirA/f2.txt", "c2\n");
    let base = repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "P"]);
    repo.git(&["mv", "dirA/f1.txt", "dirB/f1.txt"]);
    repo.git(&["mv", "dirA/f2.txt", "dirC/f2.txt"]);
    let p = repo.commit_all("split dirA into dirB and dirC");

    repo.branch_from("Q", &base);
    repo.write("dirA/f3.txt", "new\n");
    let q = repo.commit_all("add dirA/f3.txt");

    repo.git(&["checkout", "-q", "main"]);
    RenameSplit { repo, base, p, q }
}

/// Whether 0444 actually prevents this process from writing.
///
/// Root ignores the permission bits entirely, so the lock assertions are meaningless
/// there. That is not a reason to skip the test on a developer machine — it is the reason
/// restoring from a snapshot, not locking, is the actual defence.
pub fn permissions_are_enforced(scratch: &Path) -> bool {
    let probe = scratch.join(".permission-probe");
    std::fs::write(&probe, "x").expect("write probe");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o444)).expect("chmod probe");
    }
    let writable = std::fs::OpenOptions::new().write(true).open(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    !writable
}
