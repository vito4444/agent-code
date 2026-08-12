mod support;

use support::*;
use wkbd_vcs::ownership::{changed_paths, changed_paths_including_worktree, check_ownership};

#[test]
fn lists_paths_changed_between_two_commits() {
    let repo = TestRepo::init();
    repo.write("src/lib.rs", "fn main() {}\n");
    repo.write("README.md", "hi\n");
    let base = repo.commit_all("base");

    repo.write("src/lib.rs", "fn main() { todo!() }\n");
    repo.write("src/new.rs", "// new\n");
    let head = repo.commit_all("work");

    assert_eq!(changed_paths(&repo.path, &base, &head).unwrap(), vec!["src/lib.rs", "src/new.rs"]);
}

/// Rename detection is off, so a file moved out of another task's directory shows up
/// under both names. With detection on, git reports only the destination and the move
/// out of somebody else's area becomes invisible.
#[test]
fn a_rename_is_reported_as_both_paths() {
    let repo = TestRepo::init();
    repo.write("owned/thing.rs", "// a reasonably long file so rename detection fires\n".repeat(20).as_str());
    let base = repo.commit_all("base");

    std::fs::create_dir_all(repo.path.join("elsewhere")).unwrap();
    repo.git(&["mv", "owned/thing.rs", "elsewhere/thing.rs"]);
    let head = repo.commit_all("move it");

    assert_eq!(
        changed_paths(&repo.path, &base, &head).unwrap(),
        vec!["elsewhere/thing.rs", "owned/thing.rs"]
    );
}

/// Agents are audited while they are still working, and creating a file is the easiest
/// way to leave a declared area — so uncommitted edits and untracked files both count.
/// Ignored files do not: build output is not the agent making a claim.
#[test]
fn uncommitted_and_untracked_work_is_visible() {
    let repo = TestRepo::init();
    repo.write("src/lib.rs", "fn main() {}\n");
    repo.write(".gitignore", "target/\n");
    let base = repo.commit_all("base");

    repo.write("src/lib.rs", "fn main() { changed() }\n");
    repo.write("src/staged.rs", "// staged but not committed\n");
    repo.git(&["add", "src/staged.rs"]);
    repo.write("docs/notes.md", "untracked\n");
    repo.write("target/debug/artifact", "ignored\n");

    let changed = changed_paths_including_worktree(&repo.path, &base).unwrap();
    assert_eq!(changed, vec!["docs/notes.md", "src/lib.rs", "src/staged.rs"]);
    assert!(!changed.iter().any(|p| p.starts_with("target/")), "ignored output must not count");
}

#[test]
fn declarations_are_checked_against_what_really_changed() {
    let repo = TestRepo::init();
    repo.write("crates/api/src/lib.rs", "// api\n");
    repo.write("crates/ui/src/lib.rs", "// ui\n");
    repo.write("Cargo.toml", "[workspace]\n");
    let base = repo.commit_all("base");

    // The task said it would only touch the api crate, and then edited the workspace
    // manifest and another crate as well. This is the common planning failure.
    repo.write("crates/api/src/lib.rs", "// api, changed\n");
    repo.write("crates/api/src/route.rs", "// new file, still inside the declaration\n");
    repo.write("Cargo.toml", "[workspace]\nresolver = \"2\"\n");
    repo.write("crates/ui/src/lib.rs", "// ui, changed\n");
    let head = repo.commit_all("work");

    let actual = changed_paths(&repo.path, &base, &head).unwrap();
    let verdict = check_ownership(&["crates/api/**".to_string()], &actual);

    assert!(!verdict.ok());
    assert_eq!(verdict.violations, vec!["Cargo.toml", "crates/ui/src/lib.rs"]);
    assert_eq!(verdict.within, vec!["crates/api/src/lib.rs", "crates/api/src/route.rs"]);

    // With the declaration the task should have made, the same diff is clean.
    let honest = check_ownership(
        &["crates/api/".to_string(), "Cargo.toml".to_string(), "crates/ui/src/*.rs".to_string()],
        &actual,
    );
    assert!(honest.ok(), "{honest:?}");
    assert!(honest.unmatched_declarations.is_empty());
}

#[test]
fn over_declaring_is_reported_but_allowed() {
    let verdict = check_ownership(
        &["src/**".to_string(), "migrations/**".to_string()],
        &["src/lib.rs".to_string()],
    );
    assert!(verdict.ok());
    assert_eq!(verdict.unmatched_declarations, vec!["migrations/**"]);
}
