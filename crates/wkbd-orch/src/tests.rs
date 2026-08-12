use super::*;
use crate::graph::{AnyTest, DraftGraph, DraftTask, GraphProblem, VerifySpec};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn spec() -> VerifySpec {
    VerifySpec {
        setup: vec![],
        cmd: "cargo test".into(),
        must_pass: vec!["config::loads".into()],
        must_still_pass: vec![],
        immutable_paths: vec!["**/*.test.ts".into()],
    }
}

fn task(id: &str, deps: &[&str], paths: &[&str]) -> DraftTask {
    DraftTask {
        id: id.into(),
        title: format!("do {id}"),
        body: String::new(),
        declared_paths: paths.iter().map(|s| s.to_string()).collect(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        verify: spec(),
    }
}

/* ------------------------------------------------------------- graph validation */

#[test]
fn a_valid_graph_yields_a_reproducible_order_and_waves() {
    let draft = DraftGraph {
        goal: "make config fast".into(),
        tasks: vec![
            task("a", &[], &["src/config.rs"]),
            task("b", &["a"], &["src/loader.rs"]),
            task("c", &["a"], &["src/cache.rs"]),
            task("d", &["b", "c"], &["src/main.rs"]),
        ],
    };

    let validated = validate(&draft, &AnyTest).expect("valid");
    assert_eq!(validated.topological_order, vec!["a", "b", "c", "d"]);
    assert_eq!(validated.waves, vec![vec!["a"], vec!["b", "c"], vec!["d"]]);

    // Reproducible: replaying a run has to schedule in the same order or it is a different run.
    let again = validate(&draft, &AnyTest).unwrap();
    assert_eq!(validated.topological_order, again.topological_order);
}

#[test]
fn a_cycle_is_rejected_with_the_cycle_named() {
    let draft = DraftGraph {
        goal: "g".into(),
        tasks: vec![task("a", &["c"], &[]), task("b", &["a"], &[]), task("c", &["b"], &[])],
    };
    let problems = validate(&draft, &AnyTest).unwrap_err();
    assert!(problems
        .iter()
        .any(|p| matches!(p, GraphProblem::Cycle { cycle } if cycle.len() >= 3)));
}

#[test]
fn a_dangling_dependency_is_rejected() {
    let draft = DraftGraph {
        goal: "g".into(),
        tasks: vec![task("a", &["nope"], &[]), task("b", &["a"], &[])],
    };
    let problems = validate(&draft, &AnyTest).unwrap_err();
    assert!(problems.iter().any(
        |p| matches!(p, GraphProblem::UnknownDependency { depends_on, .. } if depends_on == "nope")
    ));
}

#[test]
fn all_problems_are_reported_in_one_pass() {
    // One round trip should be able to fix everything. Reporting the first problem only means
    // the model gets asked N times for a graph with N mistakes.
    let mut bad = task("a", &["a"], &[]);
    bad.title = "  ".into();
    bad.verify.must_pass.clear();
    bad.verify.must_still_pass.clear();

    let draft = DraftGraph { goal: "g".into(), tasks: vec![bad, task("a", &[], &[])] };
    let problems = validate(&draft, &AnyTest).unwrap_err();

    assert!(problems.iter().any(|p| matches!(p, GraphProblem::DuplicateTaskId { .. })));
    assert!(problems.iter().any(|p| matches!(p, GraphProblem::SelfDependency { .. })));
    assert!(problems.iter().any(|p| matches!(p, GraphProblem::EmptyTitle { .. })));
    assert!(problems.iter().any(|p| matches!(p, GraphProblem::UndecidableVerify { .. })));
}

#[test]
fn an_acceptance_spec_with_no_assertions_is_rejected() {
    // A command with no assertions cannot decide anything; it can only report that it ran. This
    // is the difference between machine-checkable acceptance and a prose test strategy.
    let mut t = task("a", &[], &[]);
    t.verify.must_pass.clear();
    t.verify.must_still_pass.clear();
    let draft = DraftGraph { goal: "g".into(), tasks: vec![t] };

    let problems = validate(&draft, &AnyTest).unwrap_err();
    assert!(problems.iter().any(|p| matches!(
        p,
        GraphProblem::UndecidableVerify { reason, .. } if reason.contains("no assertions")
    )));
}

#[test]
fn an_assertion_naming_a_test_that_does_not_exist_is_rejected() {
    struct Known(&'static [&'static str]);
    impl TestInventory for Known {
        fn contains(&self, test_id: &str) -> bool {
            self.0.contains(&test_id)
        }
    }

    let draft = DraftGraph { goal: "g".into(), tasks: vec![task("a", &[], &[])] };
    let problems = validate(&draft, &Known(&["something::else"])).unwrap_err();
    assert!(problems.iter().any(
        |p| matches!(p, GraphProblem::UnknownTest { test, .. } if test == "config::loads")
    ));

    validate(&draft, &Known(&["config::loads"])).expect("a real test id validates");
}

#[test]
fn a_task_that_both_owns_and_freezes_a_path_is_rejected() {
    let mut t = task("a", &[], &["tests/a.test.ts"]);
    t.verify.immutable_paths = vec!["tests/a.test.ts".into()];
    let draft = DraftGraph { goal: "g".into(), tasks: vec![t] };
    let problems = validate(&draft, &AnyTest).unwrap_err();
    assert!(problems
        .iter()
        .any(|p| matches!(p, GraphProblem::ImmutablePathAlsoDeclared { .. })));
}

#[test]
fn an_unconnected_task_in_a_connected_graph_is_surfaced() {
    let draft = DraftGraph {
        goal: "g".into(),
        tasks: vec![task("a", &[], &[]), task("b", &["a"], &[]), task("loose", &[], &[])],
    };
    let problems = validate(&draft, &AnyTest).unwrap_err();
    assert!(problems
        .iter()
        .any(|p| matches!(p, GraphProblem::Orphan { task } if task == "loose")));
}

#[test]
fn declared_path_overlap_is_only_a_hint() {
    let tasks = vec![
        task("a", &[], &["src/x.rs", "src/shared.rs"]),
        task("b", &[], &["src/shared.rs"]),
        task("c", &[], &["src/y.rs"]),
    ];
    let overlaps = declared_path_overlaps(&tasks);
    assert_eq!(overlaps, vec![("a".to_string(), "b".to_string())]);
    // Deliberately not asserted: that c can therefore run alongside a and b. Measured on this
    // machine, disjoint paths can still fail to merge, so only the merge decides.
}

/* ------------------------------------------------------------ durable execution */

async fn store() -> (TempDir, wkbd_store::Store) {
    let dir = TempDir::new().unwrap();
    let opened = wkbd_store::Store::open(dir.path()).unwrap();
    assert!(opened.degraded.is_none());
    (dir, opened.store)
}

#[tokio::test]
async fn a_checkpointed_step_is_never_run_again() {
    let (_d, store) = store().await;
    let wf = Workflow::start(&store, "goal", "/repo").await.unwrap();

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for _ in 0..3 {
        let calls = calls.clone();
        let value: i64 = wf
            .step("plan", || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(41)
            })
            .await
            .unwrap();
        assert_eq!(value, 41);
    }

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a step that has been checkpointed must return the recorded result rather than \
         re-running; re-running is what makes a framework's replay a fresh sample instead of \
         a reproduction"
    );
}

#[tokio::test]
async fn recovery_replays_a_workflow_and_skips_finished_steps() {
    let (_d, store) = store().await;
    let wf = Workflow::start(&store, "goal", "/repo").await.unwrap();
    let run_id = wf.run_id().to_string();

    // First pass: one step succeeds, then the process "crashes".
    let first: i64 = wf.step("a", || async { Ok(1) }).await.unwrap();
    assert_eq!(first, 1);
    drop(wf);

    // Recovery.
    let resumed = Workflow::resume(&store, &run_id);
    let a_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let ac = a_calls.clone();
    let a: i64 = resumed
        .step("a", || async move {
            ac.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(999)
        })
        .await
        .unwrap();
    let bc = b_calls.clone();
    let b: i64 = resumed
        .step("b", || async move {
            bc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(2)
        })
        .await
        .unwrap();

    assert_eq!(a, 1, "the recorded result wins over what the closure would return now");
    assert_eq!(a_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(b, 2);
    assert_eq!(b_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(resumed.completed_steps().await.unwrap(), vec!["a", "b"]);
}

#[tokio::test]
async fn a_failed_step_leaves_no_checkpoint_and_is_retried() {
    let (_d, store) = store().await;
    let wf = Workflow::start(&store, "goal", "/repo").await.unwrap();

    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let a = attempts.clone();
    let value: i64 = wf
        .step_with_retry(
            "flaky",
            RetryPolicy { max_attempts: 4, interval: std::time::Duration::from_millis(1) },
            move |_attempt| {
                let a = a.clone();
                async move {
                    let n = a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 2 {
                        anyhow::bail!("not yet")
                    }
                    Ok(7)
                }
            },
        )
        .await
        .unwrap();

    assert_eq!(value, 7);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!(wf.checkpoint("flaky").await.unwrap().is_some());
}

#[tokio::test]
async fn unfinished_runs_are_found_at_startup() {
    let (_d, store) = store().await;
    let a = Workflow::start(&store, "one", "/repo").await.unwrap();
    let b = Workflow::start(&store, "two", "/repo").await.unwrap();
    b.set_status(RunStatus::Done).await.unwrap();

    let pending = unfinished_runs(&store).await.unwrap();
    assert_eq!(pending, vec![a.run_id().to_string()]);
}

/* --------------------------------------------------------- real git integration */

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with a base commit and two independent branches.
fn repo_with_branches() -> (TempDir, std::path::PathBuf, String, String, String) {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);

    std::fs::write(repo.join("a.txt"), "line1\nline2\n").unwrap();
    std::fs::write(repo.join("b.txt"), "shared\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", "-b", "x"]);
    std::fs::write(repo.join("a.txt"), "line1-x\nline2\n").unwrap();
    git(&repo, &["commit", "-qam", "x"]);
    let x = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", &base]);
    git(&repo, &["checkout", "-q", "-b", "y"]);
    std::fs::write(repo.join("b.txt"), "shared-y\n").unwrap();
    git(&repo, &["commit", "-qam", "y"]);
    let y = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", &base]);
    (dir, repo, base, x, y)
}

#[test]
fn a_dependency_edge_puts_the_dependency_output_in_the_dependent_worktree() {
    // The property that makes an edge real. A dependent task's files already contain what its
    // dependencies produced, rather than a description of it in a prompt.
    let (dir, repo, base, x, y) = repo_with_branches();
    let worktrees = dir.path().join("wt");

    let mut commits = HashMap::new();
    commits.insert("x".to_string(), x);
    commits.insert("y".to_string(), y);

    let dependent = task("dependent", &["x", "y"], &["c.txt"]);
    let workspace =
        prepare_workspace(&repo, &worktrees, &dependent, &base, &commits).expect("prepared");

    assert_eq!(
        std::fs::read_to_string(workspace.path.join("a.txt")).unwrap(),
        "line1-x\nline2\n"
    );
    assert_eq!(std::fs::read_to_string(workspace.path.join("b.txt")).unwrap(), "shared-y\n");
}

#[test]
fn preparing_a_workspace_does_not_disturb_any_existing_worktree() {
    let (dir, repo, base, x, y) = repo_with_branches();
    let before = git(&repo, &["status", "--porcelain"]);
    let head_before = git(&repo, &["rev-parse", "HEAD"]);

    let mut commits = HashMap::new();
    commits.insert("x".to_string(), x);
    commits.insert("y".to_string(), y);
    prepare_workspace(
        &repo,
        &dir.path().join("wt"),
        &task("d", &["x", "y"], &[]),
        &base,
        &commits,
    )
    .unwrap();

    assert_eq!(git(&repo, &["status", "--porcelain"]), before);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn a_dependency_that_has_not_finished_is_refused_rather_than_guessed_at() {
    let (dir, repo, base, x, _y) = repo_with_branches();
    let mut commits = HashMap::new();
    commits.insert("x".to_string(), x);

    let err = prepare_workspace(
        &repo,
        &dir.path().join("wt"),
        &task("d", &["x", "missing"], &[]),
        &base,
        &commits,
    )
    .unwrap_err();
    assert!(matches!(err, PrepareError::DependencyNotFinished { .. }));
}

#[test]
fn a_branch_already_checked_out_elsewhere_is_refused() {
    // Both git upstream and the best known stacked-branch tool independently landed on the rule
    // that a branch in use by another worktree must not be rewritten. Discovering that from the
    // refusal is cheaper than discovering it from a corrupted worktree.
    let (dir, repo, base, _x, _y) = repo_with_branches();
    let worktrees = dir.path().join("wt");
    let t = task("solo", &[], &[]);

    prepare_workspace(&repo, &worktrees, &t, &base, &HashMap::new()).unwrap();
    let err = prepare_workspace(&repo, &worktrees.join("second"), &t, &base, &HashMap::new())
        .unwrap_err();
    assert!(matches!(err, PrepareError::BranchBusy { .. }));
}

#[test]
fn the_merge_queue_validates_the_combined_result_and_rejects_only_the_offender() {
    let (dir, repo, base, x, y) = repo_with_branches();

    // A third branch that conflicts with x.
    git(&repo, &["checkout", "-q", &base]);
    git(&repo, &["checkout", "-q", "-b", "z"]);
    std::fs::write(repo.join("a.txt"), "line1-z\nline2\n").unwrap();
    git(&repo, &["commit", "-qam", "z"]);
    let z = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", &base]);

    let entries = vec![
        QueueEntry { task_id: "x".into(), commit: x },
        QueueEntry { task_id: "y".into(), commit: y },
        QueueEntry { task_id: "z".into(), commit: z },
    ];
    let order = vec!["x".to_string(), "y".to_string(), "z".to_string()];

    let outcome = drain_merge_queue(&repo, &base, &entries, &order).unwrap();
    match outcome {
        MergeQueueOutcome::Rejected { task_id, retried_without, .. } => {
            assert_eq!(task_id, "z");
            assert_eq!(
                retried_without,
                vec!["x", "y"],
                "the entries that did combine keep their work; rolling the batch back would \
                 throw away results that were fine and produce the same failure again"
            );
        }
        other => panic!("expected z to be rejected, got {other:?}"),
    }
    let _ = dir;
}

#[test]
fn the_merge_queue_combines_everything_when_it_can() {
    let (_dir, repo, base, x, y) = repo_with_branches();
    let entries = vec![
        QueueEntry { task_id: "x".into(), commit: x },
        QueueEntry { task_id: "y".into(), commit: y },
    ];
    let outcome =
        drain_merge_queue(&repo, &base, &entries, &["x".to_string(), "y".to_string()]).unwrap();
    match outcome {
        MergeQueueOutcome::Merged { commit, order } => {
            assert_eq!(order, vec!["x", "y"]);
            let tree = git(&repo, &["show", "--stat", "--oneline", &commit]);
            assert!(!tree.is_empty());
        }
        other => panic!("expected a merge, got {other:?}"),
    }
}

#[test]
fn disjoint_paths_that_still_conflict_are_reported_as_a_conflict_without_a_file() {
    // The measured case: one branch splits a directory in two, another adds a file to the
    // original directory. No path is shared, no individual file conflicts, and the merge fails.
    // A scheduler that concluded "disjoint paths, safe to combine" would be wrong here.
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("dirA")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("dirA/f1.txt"), "1\n").unwrap();
    std::fs::write(repo.join("dirA/f2.txt"), "2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", "-b", "split"]);
    std::fs::create_dir_all(repo.join("dirB")).unwrap();
    std::fs::create_dir_all(repo.join("dirC")).unwrap();
    git(&repo, &["mv", "dirA/f1.txt", "dirB/f1.txt"]);
    git(&repo, &["mv", "dirA/f2.txt", "dirC/f2.txt"]);
    git(&repo, &["commit", "-qm", "split"]);
    let split = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", &base]);
    git(&repo, &["checkout", "-q", "-b", "add"]);
    std::fs::write(repo.join("dirA/f3.txt"), "3\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "add"]);
    let add = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", &base]);

    // The declared paths genuinely do not overlap.
    let tasks = vec![
        task("split", &[], &["dirB/f1.txt", "dirC/f2.txt"]),
        task("add", &[], &["dirA/f3.txt"]),
    ];
    assert!(declared_path_overlaps(&tasks).is_empty());

    let entries = vec![
        QueueEntry { task_id: "split".into(), commit: split },
        QueueEntry { task_id: "add".into(), commit: add },
    ];
    let outcome =
        drain_merge_queue(&repo, &base, &entries, &["split".to_string(), "add".to_string()])
            .unwrap();

    match outcome {
        MergeQueueOutcome::Rejected { task_id, detail, .. } => {
            assert_eq!(task_id, "add");
            assert!(
                detail.contains("no single conflicting file"),
                "the message must name the mechanism rather than a file, because there is no \
                 file to name: {detail}"
            );
        }
        other => panic!("expected a conflict with no conflicting file, got {other:?}"),
    }
}

#[test]
fn a_task_that_touches_undeclared_files_is_caught_afterwards() {
    let (dir, repo, base, _x, _y) = repo_with_branches();
    let worktrees = dir.path().join("wt");
    let t = task("scoped", &[], &["a.txt"]);
    let workspace = prepare_workspace(&repo, &worktrees, &t, &base, &HashMap::new()).unwrap();

    // Within declaration.
    std::fs::write(workspace.path.join("a.txt"), "changed\n").unwrap();
    assert!(check_ownership(&workspace.path, &t, &workspace.start_commit)
        .unwrap()
        .is_none());

    // Outside it.
    std::fs::write(workspace.path.join("b.txt"), "also changed\n").unwrap();
    let trigger = check_ownership(&workspace.path, &t, &workspace.start_commit)
        .unwrap()
        .expect("must be caught");
    match trigger {
        ReplanTrigger::OwnershipViolation { unexpected, .. } => {
            assert_eq!(unexpected, vec!["b.txt"]);
        }
        other => panic!("expected an ownership violation, got {other:?}"),
    }
}

/* ------------------------------------------------------------------ acceptance */

struct FixedParser(Option<Vec<String>>);

impl ResultParser for FixedParser {
    fn passing_tests(&self, _stdout: &str, _stderr: &str) -> Option<Vec<String>> {
        self.0.clone()
    }
}

#[test]
fn acceptance_passes_only_when_every_assertion_holds() {
    let dir = TempDir::new().unwrap();
    let spec = VerifySpec {
        setup: vec![],
        cmd: "true".into(),
        must_pass: vec!["new_behaviour".into()],
        must_still_pass: vec!["old_behaviour".into()],
        immutable_paths: vec![],
    };

    let both = FixedParser(Some(vec!["new_behaviour".into(), "old_behaviour".into()]));
    let report = Verifier { spec: &spec, parser: &both, timeout: std::time::Duration::from_secs(10) }
        .run(dir.path(), "patch")
        .unwrap();
    assert!(report.outcome.passed());

    // The change works but broke something else. Exit code zero would have called this a pass.
    let regressed = FixedParser(Some(vec!["new_behaviour".into()]));
    let report =
        Verifier { spec: &spec, parser: &regressed, timeout: std::time::Duration::from_secs(10) }
            .run(dir.path(), "patch")
            .unwrap();
    match report.outcome {
        VerifyOutcome::Failed { missing_pass, regressed } => {
            assert!(missing_pass.is_empty());
            assert_eq!(regressed, vec!["old_behaviour"]);
        }
        other => panic!("expected a regression, got {other:?}"),
    }
}

#[test]
fn a_runner_that_did_not_finish_is_inconclusive_not_a_failure() {
    // An empty pass list from an incomplete run must not be read as "the tests failed": the two
    // call for different responses, and treating a crashed runner as a failing test sends the
    // orchestrator to replan a task that was never actually evaluated.
    let dir = TempDir::new().unwrap();
    let spec = VerifySpec {
        setup: vec![],
        cmd: "true".into(),
        must_pass: vec!["x".into()],
        must_still_pass: vec![],
        immutable_paths: vec![],
    };
    let report =
        Verifier { spec: &spec, parser: &FixedParser(None), timeout: std::time::Duration::from_secs(5) }
            .run(dir.path(), "patch")
            .unwrap();
    assert!(matches!(report.outcome, VerifyOutcome::Inconclusive { .. }));
}

#[test]
fn a_failing_setup_is_reported_separately_from_a_failing_test() {
    let dir = TempDir::new().unwrap();
    let spec = VerifySpec {
        setup: vec!["exit 3".into()],
        cmd: "true".into(),
        must_pass: vec!["x".into()],
        must_still_pass: vec![],
        immutable_paths: vec![],
    };
    let report = Verifier {
        spec: &spec,
        parser: &FixedParser(Some(vec![])),
        timeout: std::time::Duration::from_secs(5),
    }
    .run(dir.path(), "patch")
    .unwrap();
    match report.outcome {
        VerifyOutcome::SetupFailed { code, .. } => assert_eq!(code, Some(3)),
        other => panic!("expected a setup failure, got {other:?}"),
    }
}

#[test]
fn the_patch_hash_is_part_of_the_report_so_a_cache_cannot_replay_a_different_patch() {
    let dir = TempDir::new().unwrap();
    let spec = VerifySpec {
        setup: vec![],
        cmd: "true".into(),
        must_pass: vec!["x".into()],
        must_still_pass: vec![],
        immutable_paths: vec![],
    };
    let v = Verifier {
        spec: &spec,
        parser: &FixedParser(Some(vec!["x".into()])),
        timeout: std::time::Duration::from_secs(5),
    };
    let a = v.run(dir.path(), "patch one").unwrap();
    let b = v.run(dir.path(), "patch two").unwrap();
    assert_ne!(
        a.patch_hash, b.patch_hash,
        "the cache key must include what is being verified; a harness keyed only on a run id \
         will return a previous verdict for a different patch"
    );
}

#[test]
fn the_cargo_parser_needs_a_summary_line_before_it_believes_anything() {
    let p = CargoTestParser;
    assert_eq!(p.passing_tests("test a::b ... ok\n", ""), None);
    assert_eq!(
        p.passing_tests("test a::b ... ok\ntest result: ok. 1 passed;\n", ""),
        Some(vec!["a::b".to_string()])
    );
}

#[test]
fn an_acceptance_command_that_hangs_is_killed() {
    let dir = TempDir::new().unwrap();
    let spec = VerifySpec {
        setup: vec![],
        cmd: "sleep 30".into(),
        must_pass: vec!["x".into()],
        must_still_pass: vec![],
        immutable_paths: vec![],
    };
    let err = Verifier {
        spec: &spec,
        parser: &FixedParser(Some(vec![])),
        timeout: std::time::Duration::from_millis(200),
    }
    .run(dir.path(), "patch")
    .unwrap_err();
    assert!(format!("{err}").contains("timed out"));
}
