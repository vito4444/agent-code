mod support;

use std::time::Duration;

use support::*;
use wkbd_vcs::error::VcsError;
use wkbd_vcs::worktree::{
    add_worktree, hydrate, is_branch_checked_out, list_worktrees, remove_worktree, HydrationSpec,
    SetupCommand, SkipReason,
};

fn seeded_repo() -> (TestRepo, String) {
    let repo = TestRepo::init();
    repo.write("a.txt", "base\n");
    let base = repo.commit_all("base");
    (repo, base)
}

#[test]
fn adds_a_worktree_on_a_new_branch() {
    let (repo, base) = seeded_repo();
    let path = repo.sibling("wt-one");

    let wt = add_worktree(&repo.path, &path, "task-one", &base).expect("add");
    assert_eq!(wt.head, base);
    assert_eq!(read_under(&path, "a.txt"), "base\n");
    assert_eq!(rev_parse(&path, "HEAD"), base);
    assert_eq!(
        repo.git_in(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task-one"
    );
}

/// Every attribute the porcelain format can emit, read back from a real repository:
/// a branch, a detached head, a lock with a reason, and — from a separate clone — a bare
/// repository, whose record has no `HEAD` line at all.
#[test]
fn porcelain_parsing_covers_detached_locked_and_bare() {
    let (repo, base) = seeded_repo();
    let attached = repo.sibling("wt-attached");
    let detached = repo.sibling("wt-detached");
    let locked = repo.sibling("wt-locked");

    add_worktree(&repo.path, &attached, "attached", &base).expect("add attached");
    repo.git(&["worktree", "add", "-q", "--detach", detached.to_str().unwrap(), &base]);
    add_worktree(&repo.path, &locked, "locked-branch", &base).expect("add locked");
    repo.git(&["worktree", "lock", "--reason", "busy testing", locked.to_str().unwrap()]);

    let listed = list_worktrees(&repo.path).expect("list");
    assert_eq!(listed.len(), 4, "{listed:#?}");

    let main = listed.iter().find(|w| w.path == repo.path).expect("main worktree");
    assert_eq!(main.branch_name(), Some("main"));
    assert!(!main.detached && !main.bare && !main.locked);

    let det = listed.iter().find(|w| w.path == detached).expect("detached worktree");
    assert!(det.detached);
    assert!(det.branch.is_none());
    assert_eq!(det.head.as_deref(), Some(base.as_str()));

    let lock = listed.iter().find(|w| w.path == locked).expect("locked worktree");
    assert!(lock.locked);
    assert_eq!(lock.lock_reason.as_deref(), Some("busy testing"));
    assert_eq!(lock.branch.as_deref(), Some("refs/heads/locked-branch"));

    let bare_path = repo.sibling("bare.git");
    repo.git_in(
        repo.dir.path(),
        &["clone", "--bare", "-q", repo.path.to_str().unwrap(), bare_path.to_str().unwrap()],
    );
    let bare = list_worktrees(&bare_path).expect("list bare");
    assert_eq!(bare.len(), 1);
    assert!(bare[0].bare);
    assert!(bare[0].head.is_none() && bare[0].branch.is_none());
}

/// The scheduler asks this before it moves a branch. git enforces the same rule for
/// checkout, but not for `git branch -f` or `git reset`, which is what makes an explicit
/// check necessary.
#[test]
fn reports_where_a_branch_is_checked_out() {
    let (repo, base) = seeded_repo();
    let path = repo.sibling("wt-taken");
    add_worktree(&repo.path, &path, "taken", &base).expect("add");

    assert_eq!(is_branch_checked_out(&repo.path, "taken").unwrap().as_deref(), Some(path.as_path()));
    assert_eq!(
        is_branch_checked_out(&repo.path, "refs/heads/taken").unwrap().as_deref(),
        Some(path.as_path())
    );
    assert_eq!(is_branch_checked_out(&repo.path, "main").unwrap().as_deref(), Some(repo.path.as_path()));
    assert_eq!(is_branch_checked_out(&repo.path, "never-created").unwrap(), None);

    // And the same rule as a refusal: a second worktree cannot take the branch.
    let second = repo.sibling("wt-second");
    match add_worktree(&repo.path, &second, "taken", &base) {
        Err(VcsError::BranchAlreadyCheckedOut { branch, path: at }) => {
            assert_eq!(branch, "taken");
            assert_eq!(at, path);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(!second.exists(), "nothing should have been created");
}

#[test]
fn refuses_to_reuse_an_existing_branch_name() {
    let (repo, base) = seeded_repo();
    repo.git(&["branch", "already-there", &base]);
    let path = repo.sibling("wt-existing");

    match add_worktree(&repo.path, &path, "already-there", &base) {
        Err(VcsError::BranchExists { branch }) => assert_eq!(branch, "already-there"),
        other => panic!("expected BranchExists, got {other:?}"),
    }
}

#[test]
fn removal_protects_uncommitted_work() {
    let (repo, base) = seeded_repo();
    let path = repo.sibling("wt-dirty");
    add_worktree(&repo.path, &path, "dirty", &base).expect("add");
    write_under(&path, "a.txt", "an agent was in the middle of this\n");

    match remove_worktree(&repo.path, &path, false) {
        Err(VcsError::WorktreeNotClean { path: at }) => assert_eq!(at, path),
        other => panic!("expected WorktreeNotClean, got {other:?}"),
    }
    assert!(path.exists());

    remove_worktree(&repo.path, &path, true).expect("forced removal");
    assert!(!path.exists());
    assert_eq!(list_worktrees(&repo.path).unwrap().len(), 1);
}

/// Hydration is the difference between a worktree that has the code and a worktree that
/// can run it. It must also refuse the two shapes that turn a copy into an exfiltration:
/// a symlink pointing out of the source tree, and a pattern that names something outside
/// it.
#[test]
fn hydration_copies_declared_files_and_refuses_symlinks() {
    let (repo, base) = seeded_repo();

    // The user's checkout: tracked code plus the untracked files a fresh worktree lacks.
    let source = repo.path.clone();
    write_under(&source, ".env", "DATABASE_URL=postgres://localhost/dev\n");
    write_under(&source, "config/local.json", "{\"port\":5173}\n");
    write_under(&source, "node_modules/left-pad/index.js", "module.exports=1\n");
    std::os::unix::fs::symlink("/etc/passwd", source.join("secrets.env")).expect("symlink");

    let target = repo.sibling("wt-hydrated");
    add_worktree(&repo.path, &target, "hydrated", &base).expect("add");
    assert!(!target.join(".env").exists(), "a fresh worktree starts inert");

    let spec = HydrationSpec {
        source: source.clone(),
        copy_globs: vec!["*.env".to_string(), ".env".to_string(), "config/**".to_string()],
        setup_commands: vec![SetupCommand::new("sh", ["-c", "echo hydrated > setup-ran.txt"])],
    };
    let report = hydrate(&target, &spec).expect("hydrate");

    assert_eq!(report.copied, vec![".env", "config/local.json"]);
    assert_eq!(read_under(&target, ".env"), "DATABASE_URL=postgres://localhost/dev\n");
    assert!(!target.join("node_modules").exists(), "undeclared files must not travel");

    let skipped = report.skipped.iter().find(|s| s.path == "secrets.env").expect("symlink reported");
    assert_eq!(skipped.reason, SkipReason::Symlink);
    assert!(!target.join("secrets.env").exists(), "the symlink target must not be materialised");

    assert_eq!(report.commands.len(), 1);
    assert_eq!(read_under(&target, "setup-ran.txt"), "hydrated\n");
}

#[test]
fn hydration_refuses_patterns_that_leave_the_source() {
    let (repo, base) = seeded_repo();
    let target = repo.sibling("wt-escape");
    add_worktree(&repo.path, &target, "escape", &base).expect("add");
    write_under(repo.dir.path(), "outside.txt", "not yours\n");

    for pattern in ["../outside.txt", "/etc/passwd", "~/.ssh/id_rsa", "config/../../outside.txt"] {
        let spec = HydrationSpec {
            source: repo.path.clone(),
            copy_globs: vec![pattern.to_string()],
            setup_commands: Vec::new(),
        };
        match hydrate(&target, &spec) {
            Err(VcsError::Glob { pattern: reported, .. }) => assert_eq!(reported, pattern),
            other => panic!("pattern {pattern} should have been refused, got {other:?}"),
        }
    }
    assert!(!target.join("outside.txt").exists());
}

/// The agent owns its worktree, so it can plant a symlinked directory between hydration
/// runs and have the next copy land wherever it points.
#[test]
fn hydration_will_not_write_through_a_symlinked_destination() {
    let (repo, base) = seeded_repo();
    let target = repo.sibling("wt-symlinked-dest");
    add_worktree(&repo.path, &target, "symlinked-dest", &base).expect("add");

    let elsewhere = repo.sibling("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("create elsewhere");
    std::os::unix::fs::symlink(&elsewhere, target.join("config")).expect("symlink dest");

    write_under(&repo.path, "config/local.json", "{\"port\":5173}\n");
    let spec = HydrationSpec {
        source: repo.path.clone(),
        copy_globs: vec!["config/**".to_string()],
        setup_commands: Vec::new(),
    };
    let report = hydrate(&target, &spec).expect("hydrate");

    assert!(report.copied.is_empty(), "nothing may be written through the symlink");
    assert_eq!(
        report.skipped.iter().map(|s| s.reason.clone()).collect::<Vec<_>>(),
        vec![SkipReason::EscapesRoot]
    );
    assert!(!elsewhere.join("local.json").exists());
}

#[test]
fn a_failing_setup_command_is_loud() {
    let (repo, base) = seeded_repo();
    let target = repo.sibling("wt-setup-fails");
    add_worktree(&repo.path, &target, "setup-fails", &base).expect("add");

    let spec = HydrationSpec {
        source: repo.path.clone(),
        copy_globs: Vec::new(),
        setup_commands: vec![SetupCommand::new("sh", ["-c", "echo no registry >&2; exit 3"])],
    };
    match hydrate(&target, &spec) {
        Err(VcsError::SetupCommand { detail, .. }) => {
            assert!(detail.contains("exited with 3"), "{detail}");
            assert!(detail.contains("no registry"), "{detail}");
        }
        other => panic!("expected SetupCommand, got {other:?}"),
    }
}

#[test]
fn a_hanging_setup_command_is_killed() {
    let (repo, base) = seeded_repo();
    let target = repo.sibling("wt-setup-hangs");
    add_worktree(&repo.path, &target, "setup-hangs", &base).expect("add");

    let spec = HydrationSpec {
        source: repo.path.clone(),
        copy_globs: Vec::new(),
        setup_commands: vec![SetupCommand {
            program: "sleep".to_string(),
            args: vec!["30".to_string()],
            timeout: Some(Duration::from_millis(200)),
        }],
    };
    let started = std::time::Instant::now();
    match hydrate(&target, &spec) {
        Err(VcsError::SetupCommand { detail, .. }) => assert!(detail.contains("timed out"), "{detail}"),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(5), "the timeout did not fire");
}
