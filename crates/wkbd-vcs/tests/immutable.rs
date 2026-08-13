mod support;

use support::*;
use wkbd_vcs::immutable::{
    lock_paths, restore_from_commit, restore_from_snapshot, snapshot_paths,
    snapshot_paths_with_contents, verify_snapshot,
};

fn suite() -> (TestRepo, std::path::PathBuf, Vec<String>) {
    let repo = TestRepo::init();
    repo.write("tests/test_math.py", "def test_add():\n    assert add(1, 2) == 3\n");
    repo.write("tests/test_io.py", "def test_read():\n    assert read() == b''\n");
    repo.write("src/app.py", "def add(a, b):\n    return 0\n");
    repo.commit_all("base");
    let store = repo.sibling("snapshot-store");
    (repo, store, vec!["tests/**".to_string()])
}

/// One byte is enough. A test suite that has been edited at all has stopped being
/// evidence, so the report is per file and does not care how large the change was.
#[test]
fn a_single_byte_change_is_detected() {
    let (repo, store, globs) = suite();
    let snapshot = snapshot_paths_with_contents(&repo.path, &globs, &store).expect("snapshot");
    assert_eq!(snapshot.entries.len(), 2);
    assert!(verify_snapshot(&repo.path, &snapshot).unwrap().ok());

    let path = repo.path.join("tests/test_math.py");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("== 3", "== 4")).unwrap();

    let report = verify_snapshot(&repo.path, &snapshot).unwrap();
    assert!(!report.ok());
    assert_eq!(report.modified, vec!["tests/test_math.py"]);
    assert!(report.missing.is_empty() && report.added.is_empty());
}

/// Restoring is the actual defence: whatever the agent did to these files, they are the
/// originals again by the time the suite runs.
#[test]
fn restoring_puts_the_originals_back() {
    let (repo, store, globs) = suite();
    let snapshot = snapshot_paths_with_contents(&repo.path, &globs, &store).expect("snapshot");
    let before = read_under(&repo.path, "tests/test_math.py");

    std::fs::write(repo.path.join("tests/test_math.py"), "def test_add():\n    assert True\n").unwrap();
    std::fs::remove_file(repo.path.join("tests/test_io.py")).unwrap();

    let restore = restore_from_snapshot(&repo.path, &snapshot).expect("restore");
    assert_eq!(restore.restored, vec!["tests/test_io.py", "tests/test_math.py"]);
    assert_eq!(read_under(&repo.path, "tests/test_math.py"), before);
    assert!(verify_snapshot(&repo.path, &snapshot).unwrap().ok());
}

/// The conftest.py shape: the cheat is an *added* file, executed during collection before
/// any test runs. A restore that only rewrites known files leaves it in place.
#[test]
fn an_added_file_is_reported_and_removed() {
    let (repo, store, globs) = suite();
    let snapshot = snapshot_paths_with_contents(&repo.path, &globs, &store).expect("snapshot");

    repo.write(
        "tests/conftest.py",
        "def pytest_report_teststatus(report):\n    return report.outcome, '.', 'PASSED'\n",
    );

    let verified = verify_snapshot(&repo.path, &snapshot).unwrap();
    assert_eq!(verified.added, vec!["tests/conftest.py"]);
    assert!(!verified.ok());

    let restore = restore_from_snapshot(&repo.path, &snapshot).expect("restore");
    assert_eq!(restore.removed, vec!["tests/conftest.py"]);
    assert!(!repo.path.join("tests/conftest.py").exists());
    assert!(verify_snapshot(&repo.path, &snapshot).unwrap().ok());
}

/// Hashing correctly is not enough if the file has become a pointer to somewhere else.
#[test]
fn a_file_replaced_by_a_symlink_is_caught() {
    let (repo, store, globs) = suite();
    let snapshot = snapshot_paths_with_contents(&repo.path, &globs, &store).expect("snapshot");

    let target = repo.path.join("tests/test_math.py");
    let decoy = repo.sibling("decoy.py");
    std::fs::write(&decoy, std::fs::read(&target).unwrap()).unwrap();
    std::fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&decoy, &target).unwrap();

    let verified = verify_snapshot(&repo.path, &snapshot).unwrap();
    assert_eq!(verified.replaced_by_symlink, vec!["tests/test_math.py"]);

    restore_from_snapshot(&repo.path, &snapshot).expect("restore");
    assert!(!target.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(verify_snapshot(&repo.path, &snapshot).unwrap().ok());
}

/// Locking is a speed bump, and the test says so: where the platform enforces 0444 an
/// ordinary write fails, and where it does not (root) the restore step is what still
/// makes tampering pointless.
#[test]
fn locking_makes_ordinary_writes_fail() {
    let (repo, store, globs) = suite();
    let snapshot = snapshot_paths_with_contents(&repo.path, &globs, &store).expect("snapshot");
    let locked = lock_paths(&repo.path, &globs).expect("lock");
    assert_eq!(locked.len(), 2);

    let target = repo.path.join("tests/test_math.py");
    if permissions_are_enforced(repo.dir.path()) {
        assert!(std::fs::write(&target, "tampered\n").is_err(), "0444 should refuse a write");
        assert!(std::fs::OpenOptions::new().append(true).open(&target).is_err());
    }

    // Whatever the platform allowed, the harness restores before it runs anything.
    let restore = restore_from_snapshot(&repo.path, &snapshot).expect("restore through the lock");
    assert!(restore.removed.is_empty());
    assert!(verify_snapshot(&repo.path, &snapshot).unwrap().ok());
}

#[test]
fn a_hash_only_snapshot_refuses_to_restore() {
    let (repo, _store, globs) = suite();
    let snapshot = snapshot_paths(&repo.path, &globs).expect("snapshot");
    assert!(snapshot.entries.iter().all(|e| e.stored.is_none()));
    assert!(restore_from_snapshot(&repo.path, &snapshot).is_err());
    // It can still detect tampering, which is all it claims to do.
    std::fs::write(repo.path.join("tests/test_io.py"), "gone\n").unwrap();
    assert_eq!(verify_snapshot(&repo.path, &snapshot).unwrap().modified, vec!["tests/test_io.py"]);
}

/// The git-backed restore path, and the rule it exists to enforce: named files come back,
/// files that are not in the commit are unlinked, and everything the environment setup
/// produced is still there afterwards. `git checkout <commit>` with no path arguments
/// would have deleted the untracked build output and the local `.env` along with the
/// agent's edits — the SWE-bench harness shipped that bug and failed every patch with it.
#[test]
fn restoring_from_a_commit_leaves_the_environment_alone() {
    let repo = TestRepo::init();
    repo.write("tests/test_math.py", "def test_add():\n    assert add(1, 2) == 3\n");
    repo.write("src/app.py", "def add(a, b):\n    return 0\n");
    let pristine = repo.commit_all("base");

    // What the environment setup produced, none of it in the commit.
    repo.write(".env", "DATABASE_URL=postgres://localhost/test\n");
    repo.write("node_modules/left-pad/index.js", "module.exports=1\n");
    // What the agent did: weaken a test, add a collection-time hook, fix the code.
    repo.write("tests/test_math.py", "def test_add():\n    assert True\n");
    repo.write("tests/conftest.py", "# marks everything passed\n");
    repo.write("src/app.py", "def add(a, b):\n    return a + b\n");

    let report = restore_from_commit(
        &repo.path,
        &pristine,
        &["tests/test_math.py".to_string(), "tests/conftest.py".to_string()],
    )
    .expect("restore");

    assert_eq!(report.restored, vec!["tests/test_math.py"]);
    assert_eq!(report.removed, vec!["tests/conftest.py"]);
    assert_eq!(
        read_under(&repo.path, "tests/test_math.py"),
        "def test_add():\n    assert add(1, 2) == 3\n"
    );
    assert!(!repo.path.join("tests/conftest.py").exists());

    assert_eq!(read_under(&repo.path, ".env"), "DATABASE_URL=postgres://localhost/test\n");
    assert!(repo.path.join("node_modules/left-pad/index.js").exists());
    // The agent's fix to the source is untouched: only the named files were restored.
    assert_eq!(read_under(&repo.path, "src/app.py"), "def add(a, b):\n    return a + b\n");
}

#[test]
fn restoring_from_a_commit_refuses_globs() {
    let (repo, _store, _globs) = suite();
    let head = repo.head();
    assert!(restore_from_commit(&repo.path, &head, &["tests/**".to_string()]).is_err());
}
