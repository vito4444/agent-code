mod support;

use std::collections::BTreeSet;

use support::*;
use wkbd_vcs::error::VcsError;
use wkbd_vcs::merge::{create_integration_commit, predict_merge};
use wkbd_vcs::ownership::changed_paths;

/// Two branches editing different files: clean, with a real tree written to the object
/// database and nothing to report.
#[test]
fn clean_merge_is_reported_clean() {
    let repo = TestRepo::init();
    repo.write("a.txt", "line1\nline2\n");
    repo.write("shared.txt", "shared\n");
    let base = repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "x"]);
    repo.write("a.txt", "line1-X\nline2\n");
    let x = repo.commit_all("x");

    repo.branch_from("y", &base);
    repo.write("shared.txt", "shared-Y\n");
    let y = repo.commit_all("y");

    let prediction = predict_merge(&repo.path, &x, &y).expect("predict");
    assert!(prediction.clean, "expected a clean merge, got {prediction:?}");
    assert!(prediction.conflicted_paths.is_empty());
    assert_eq!(prediction.tree_oid.len(), 40);
    // The tree is a real object, not just a printed string.
    assert_eq!(
        repo.git(&["cat-file", "-t", &prediction.tree_oid]),
        "tree"
    );
}

/// Both branches edit the same line. The conflict is reported per stage: base, ours,
/// theirs.
#[test]
fn content_conflict_lists_the_conflicted_path() {
    let repo = TestRepo::init();
    repo.write("a.txt", "line1\nline2\n");
    let base = repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "x"]);
    repo.write("a.txt", "line1-X\nline2\n");
    let x = repo.commit_all("x");

    repo.branch_from("z", &base);
    repo.write("a.txt", "line1-Z\nline2\n");
    let z = repo.commit_all("z");

    let prediction = predict_merge(&repo.path, &x, &z).expect("predict");
    assert!(!prediction.clean);
    assert!(!prediction.conflicted_paths.is_empty(), "conflicted paths must be reported");
    assert_eq!(prediction.conflicted_files(), vec!["a.txt"]);
    assert_eq!(
        prediction.conflicted_paths.iter().map(|c| c.stage).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        prediction.messages.iter().any(|m| m.contains("CONFLICT (content)")),
        "messages: {:?}",
        prediction.messages
    );
}

/// The regression that the whole scheduling design rests on.
///
/// The two branches change disjoint sets of paths, so any scheduler that decides
/// parallelism by intersecting declared paths calls this pair safe. git does not: the
/// merge conflicts, and it conflicts with an **empty** conflicted-file list, so a parser
/// that infers cleanliness from "nothing was reported" gets it exactly backwards. Only
/// the exit code carries the answer.
#[test]
fn directory_rename_split_conflicts_with_no_conflicted_files() {
    let fixture = directory_rename_split();
    let repo = &fixture.repo;

    let p_changed: BTreeSet<String> =
        changed_paths(&repo.path, &fixture.base, &fixture.p).unwrap().into_iter().collect();
    let q_changed: BTreeSet<String> =
        changed_paths(&repo.path, &fixture.base, &fixture.q).unwrap().into_iter().collect();
    assert!(
        p_changed.intersection(&q_changed).next().is_none(),
        "fixture is wrong: {p_changed:?} and {q_changed:?} overlap"
    );

    let prediction = predict_merge(&repo.path, &fixture.p, &fixture.q).expect("predict");

    assert!(!prediction.clean, "disjoint paths still conflict: {prediction:?}");
    assert!(
        prediction.conflicted_paths.is_empty(),
        "this conflict has no individual file to blame, got {:?}",
        prediction.conflicted_paths
    );
    assert!(
        prediction.messages.iter().any(|m| m.contains("directory rename split")),
        "messages: {:?}",
        prediction.messages
    );
}

/// A dependency edge has to move code, not just a sentence in a prompt: a task that
/// depends on two others must start from a tree that already contains both results.
#[test]
fn integration_commit_carries_every_dependency() {
    let repo = TestRepo::init();
    repo.write("a.txt", "line1\nline2\n");
    repo.write("shared.txt", "shared\n");
    let base = repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "task-x"]);
    repo.write("a.txt", "line1-X\nline2\n");
    let x = repo.commit_all("x");

    repo.branch_from("task-y", &base);
    repo.write("shared.txt", "shared-Y\n");
    let y = repo.commit_all("y");

    repo.git(&["checkout", "-q", "main"]);
    let integration =
        create_integration_commit(&repo.path, &[&x, &y], "integration for task-b").expect("integrate");

    let dependent = repo.sibling("wt-task-b");
    repo.git(&["worktree", "add", "-q", "-b", "task-b", dependent.to_str().unwrap(), &integration]);

    assert_eq!(read_under(&dependent, "a.txt"), "line1-X\nline2\n");
    assert_eq!(read_under(&dependent, "shared.txt"), "shared-Y\n");
    let parents = repo.git(&["show", "-s", "--format=%P", &integration]);
    assert_eq!(parents.split_whitespace().collect::<Vec<_>>(), vec![x.as_str(), y.as_str()]);
}

/// Three dependencies fold pairwise, in order, and the result still contains all three.
#[test]
fn three_parents_fold_pairwise() {
    let repo = TestRepo::init();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");

    let mut tips = Vec::new();
    for name in ["one", "two", "three"] {
        repo.branch_from(name, &base);
        repo.write(&format!("{name}.txt"), name);
        tips.push(repo.commit_all(name));
    }
    repo.git(&["checkout", "-q", "main"]);

    let refs: Vec<&str> = tips.iter().map(String::as_str).collect();
    let integration = create_integration_commit(&repo.path, &refs, "integrate three").expect("integrate");

    let wt = repo.sibling("wt-three");
    repo.git(&["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), &integration]);
    for name in ["one", "two", "three"] {
        assert_eq!(read_under(&wt, &format!("{name}.txt")), name);
    }
}

/// A conflicting integration is an outcome the scheduler acts on, so it arrives as
/// structured data rather than as a string to grep.
#[test]
fn integration_refuses_to_commit_a_conflict() {
    let fixture = directory_rename_split();
    let err = create_integration_commit(&fixture.repo.path, &[&fixture.p, &fixture.q], "nope")
        .expect_err("must refuse");
    match err {
        VcsError::MergeConflict { prediction, .. } => {
            assert!(!prediction.clean);
            assert!(prediction.messages.iter().any(|m| m.contains("directory rename split")));
        }
        other => panic!("expected MergeConflict, got {other:?}"),
    }
}

/// `merge-tree` exits 1 for a bad commit-ish as well as for a conflict. Mapping "exit 1"
/// straight onto "conflict" would report a typo as a pair that can never be merged.
#[test]
fn an_unknown_ref_is_an_error_not_a_conflict() {
    let repo = TestRepo::init();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");

    let err = predict_merge(&repo.path, &base, "no-such-branch").expect_err("must fail");
    assert!(
        !matches!(err, VcsError::MergeConflict { .. }),
        "a missing ref must not look like a conflict: {err:?}"
    );
}

/// The reason merge prediction is usable at all: agents keep working while the
/// orchestrator evaluates combinations of their output. Prediction and integration must
/// leave every checkout — including a dirty one — byte for byte as they found it.
#[test]
fn prediction_and_integration_touch_no_working_tree() {
    let repo = TestRepo::init();
    repo.write("a.txt", "line1\n");
    repo.write("shared.txt", "shared\n");
    let base = repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "task-x"]);
    repo.write("a.txt", "line1-X\n");
    let x = repo.commit_all("x");

    repo.branch_from("task-y", &base);
    repo.write("shared.txt", "shared-Y\n");
    let y = repo.commit_all("y");

    repo.git(&["checkout", "-q", "main"]);

    // A second worktree standing in for a still-running agent: mid-edit, with staged,
    // unstaged and untracked changes.
    let agent_wt = repo.sibling("wt-agent");
    repo.git(&["worktree", "add", "-q", "-b", "agent", agent_wt.to_str().unwrap(), &base]);
    write_under(&agent_wt, "a.txt", "half-finished\n");
    write_under(&agent_wt, "scratch.txt", "untracked\n");
    repo.git_in(&agent_wt, &["add", "scratch.txt"]);
    write_under(&agent_wt, "another.txt", "also untracked\n");

    // And a dirty main checkout, because users do not stop working either.
    repo.write("a.txt", "user is editing this\n");

    let before = (
        status_of(&repo.path),
        status_of(&agent_wt),
        rev_parse(&repo.path, "HEAD"),
        rev_parse(&agent_wt, "HEAD"),
        read_under(&repo.path, "a.txt"),
        read_under(&agent_wt, "a.txt"),
    );

    let prediction = predict_merge(&repo.path, &x, &y).expect("predict");
    assert!(prediction.clean);
    let integration = create_integration_commit(&repo.path, &[&x, &y], "integration").expect("integrate");
    assert_eq!(repo.git(&["cat-file", "-t", &integration]), "commit");

    let after = (
        status_of(&repo.path),
        status_of(&agent_wt),
        rev_parse(&repo.path, "HEAD"),
        rev_parse(&agent_wt, "HEAD"),
        read_under(&repo.path, "a.txt"),
        read_under(&agent_wt, "a.txt"),
    );
    assert_eq!(before, after, "a working tree changed during merge prediction");
}
