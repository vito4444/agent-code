//! Turning something that worked three times into something reusable.
//!
//! # The gate is an external signal, and only an external signal
//!
//! The tempting design is to ask a model which of its runs went well and distil those.
//! That is intrinsic self-correction, and when it was measured without oracle labels
//! (ICLR 2024) accuracy *fell* on every benchmark tested. The reported gains in earlier
//! work came from using the ground-truth label to decide when to stop revising, which is
//! a methodological flaw rather than a technique: the oracle was doing the work.
//!
//! So a run only counts here if something outside the model said it worked — a test
//! command that passed, a command that exited zero. [`Signal::ModelSelfReport`] exists so
//! that the refusal is explicit and testable rather than implicit in the absence of a
//! variant: it is neither success nor failure, and a pile of them distils to nothing.
//!
//! # Repetition is the second half of the gate
//!
//! One verified success is a coincidence. The same step sequence succeeding
//! [`DistillPolicy::min_occurrences`] times, with a verified signal every time and no
//! failures anywhere in the group, is a procedure. A group where the same steps sometimes
//! failed is disqualified outright rather than averaged: a procedure that works most of
//! the time is exactly the thing that is worst to write down, because it will be followed
//! on the occasions when it does not.
//!
//! # And it still has to be approved
//!
//! Distillation produces [`crate::proposals`] entries, never playbook bullets. The whole
//! point of the proposals module is that the system does not get to write its own future
//! instructions unattended, and a distiller that wrote directly to the playbook would be
//! precisely the bypass that makes the review step decorative.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use wkbd_store::Store;

use crate::proposals::{
    self, ApprovalBudget, Created, Evidence, NewProposal, ProposalPayload,
};

/// How many independent verified successes a procedure needs.
///
/// Independent is the load-bearing word, and it is counted in distinct
/// [`RunSummary::occasion`]s rather than in observations.
pub const DEFAULT_MIN_OCCURRENCES: usize = 3;

/// Something that happened at the end of a run, and who says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum Signal {
    /// A verification command reported success.
    TestsPassed { command: String },
    /// A command ran to completion with this exit status.
    CommandExited { command: String, code: i32 },
    /// The model's own account of how it went. Deliberately representable, and
    /// deliberately worth nothing: see the module docs.
    ModelSelfReport { claim: String },
}

impl Signal {
    pub fn is_verified_success(&self) -> bool {
        match self {
            Signal::TestsPassed { .. } => true,
            Signal::CommandExited { code, .. } => *code == 0,
            Signal::ModelSelfReport { .. } => false,
        }
    }

    pub fn is_failure(&self) -> bool {
        match self {
            Signal::TestsPassed { .. } => false,
            Signal::CommandExited { code, .. } => *code != 0,
            Signal::ModelSelfReport { .. } => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Signal::TestsPassed { command } => format!("{command} passed"),
            Signal::CommandExited { command, code } => format!("{command} exited {code}"),
            Signal::ModelSelfReport { claim } => format!("self-reported: {claim}"),
        }
    }
}

/// What a finished run looked like, reduced to the parts distillation can use.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    /// What the evidence cites. Free-form, and finer-grained than [`Self::occasion`] where the
    /// caller has something more precise to point a reviewer at.
    pub run_id: String,
    /// The independent occasion this observation belongs to.
    ///
    /// Two observations that share one count once toward the repetition gate, and this is the
    /// distinction the gate rests on. An orchestrated run splits a goal into tasks that often
    /// resemble each other, so three sibling tasks succeeding the same way is one planner making
    /// one decision that worked — not three occasions on which a procedure proved itself. Counting
    /// them as three would let a single run write a procedure into the playbook, which is the
    /// coincidence this module exists to refuse.
    pub occasion: String,
    /// Project root, and the scope any resulting bullet would live in.
    pub scope: String,
    /// A coarse class of task. Two runs are only ever compared within one class.
    pub goal_kind: String,
    /// Step signatures in order — the tool and its target, normalised by the caller, not
    /// prose. Prose would make every run unique and nothing would ever repeat.
    pub steps: Vec<String>,
    pub signals: Vec<Signal>,
}

impl RunSummary {
    fn verified(&self) -> bool {
        self.signals.iter().any(Signal::is_verified_success)
    }

    fn failed(&self) -> bool {
        self.signals.iter().any(Signal::is_failure)
    }

    fn signature(&self) -> String {
        format!("{}\u{1}{}\u{1}{}", self.scope, self.goal_kind, self.steps.join("\u{2}"))
    }
}

#[derive(Debug, Clone)]
pub struct DistillPolicy {
    pub min_occurrences: usize,
    /// Below this, a procedure is not one.
    ///
    /// A single step is a fact about a task, not a sequence worth remembering, and it lands in
    /// the playbook as a bullet reading `Workflow "...": edit *.rs` — which says nothing an agent
    /// about to edit a Rust file does not already know. The playbook is injected into every
    /// session, so a bullet that teaches nothing is not free: it is a line of the prelude spent
    /// making the rest of it less prominent.
    pub min_steps: usize,
    /// Procedures longer than this are not distilled. A twenty-step recipe is a
    /// description of one run rather than a reusable one, and it will be wrong by the time
    /// it is read.
    pub max_steps: usize,
}

impl Default for DistillPolicy {
    fn default() -> Self {
        DistillPolicy {
            min_occurrences: DEFAULT_MIN_OCCURRENCES,
            min_steps: 2,
            max_steps: 8,
        }
    }
}

/// A group of identical runs that passed the gate, before it is named.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDraft {
    pub scope: String,
    pub goal_kind: String,
    pub steps: Vec<String>,
    pub supporting_runs: Vec<String>,
    pub verified_signals: Vec<String>,
}

/// A named procedure, ready to be proposed.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowCandidate {
    pub name: String,
    pub scope: String,
    pub goal_kind: String,
    pub steps: Vec<String>,
    pub supporting_runs: Vec<String>,
    pub verified_signals: Vec<String>,
}

/// The only place a model is of any use in this module: naming things.
///
/// Naming is presentation. It cannot change what was distilled, what evidence backs it, or
/// whether it passes the gate, so a bad name costs a bad name.
pub trait StepNamer: Send + Sync {
    fn name(&self, draft: &WorkflowDraft) -> String;
}

/// Deterministic naming from the goal class and the first step.
pub struct SignatureNamer;

impl StepNamer for SignatureNamer {
    fn name(&self, draft: &WorkflowDraft) -> String {
        match draft.steps.first() {
            Some(first) => format!("{} via {} ({} steps)", draft.goal_kind, first, draft.steps.len()),
            None => draft.goal_kind.clone(),
        }
    }
}

/// Groups runs and keeps the ones that clear the gate.
pub fn distill(runs: &[RunSummary], policy: &DistillPolicy) -> Vec<WorkflowCandidate> {
    distill_with(runs, policy, &SignatureNamer)
}

pub fn distill_with(
    runs: &[RunSummary],
    policy: &DistillPolicy,
    namer: &dyn StepNamer,
) -> Vec<WorkflowCandidate> {
    // BTreeMap, so the output order is the signature order and two runs of the same input
    // produce the same proposals in the same sequence.
    let mut groups: BTreeMap<String, Vec<&RunSummary>> = BTreeMap::new();
    for run in runs {
        if run.steps.len() < policy.min_steps.max(1) || run.steps.len() > policy.max_steps {
            continue;
        }
        groups.entry(run.signature()).or_default().push(run);
    }

    let mut out = Vec::new();
    for (_, group) in groups {
        // One failure anywhere in the group disqualifies the whole procedure. Counting
        // only the successes would distil a recipe that is known to fail sometimes, and
        // it would be followed on exactly those occasions.
        if group.iter().any(|r| r.failed()) {
            continue;
        }
        let verified: Vec<&&RunSummary> = group.iter().filter(|r| r.verified()).collect();
        // Only runs whose success something outside the model attested to are cited.
        if verified.len() != group.len() {
            continue;
        }
        // Distinct occasions, not observations. Three tasks in one run that all worked are one
        // occasion on which this shape of work succeeded, and treating them as three would let a
        // single run promote a coincidence into a procedure.
        let occasions: BTreeSet<&str> =
            verified.iter().map(|r| r.occasion.as_str()).collect();
        if occasions.len() < policy.min_occurrences {
            continue;
        }

        let first = group[0];
        let draft = WorkflowDraft {
            scope: first.scope.clone(),
            goal_kind: first.goal_kind.clone(),
            steps: first.steps.clone(),
            supporting_runs: group.iter().map(|r| r.run_id.clone()).collect(),
            verified_signals: group
                .iter()
                .flat_map(|r| {
                    r.signals
                        .iter()
                        .filter(|s| s.is_verified_success())
                        .map(Signal::describe)
                })
                .collect(),
        };
        out.push(WorkflowCandidate {
            name: namer.name(&draft),
            scope: draft.scope,
            goal_kind: draft.goal_kind,
            steps: draft.steps,
            supporting_runs: draft.supporting_runs,
            verified_signals: draft.verified_signals,
        });
    }
    out
}

/// Files candidates as proposals. Nothing here writes to the playbook.
pub async fn propose(
    store: &Store,
    candidates: Vec<WorkflowCandidate>,
    budget: &ApprovalBudget,
) -> Result<Vec<Created>> {
    let mut out = Vec::new();
    for candidate in candidates {
        let evidence = Evidence {
            supporting_runs: candidate.supporting_runs.clone(),
            verified_signals: candidate.verified_signals.clone(),
            note: format!(
                "the same {} steps succeeded {} times with an external success signal each time",
                candidate.steps.len(),
                candidate.supporting_runs.len()
            ),
        };
        let payload = ProposalPayload::Workflow {
            scope: candidate.scope.clone(),
            name: candidate.name.clone(),
            steps: candidate.steps.clone(),
        };
        out.push(proposals::create(store, NewProposal::new(payload, evidence), budget).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook;
    use crate::proposals::{approve, apply, present_for_review, Confirmation, Status};
    use tempfile::TempDir;

    const SCOPE: &str = "/repo";

    async fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let opened = Store::open(dir.path()).unwrap();
        assert!(opened.degraded.is_none());
        (dir, opened.store)
    }

    fn steps() -> Vec<String> {
        vec![
            "read:Cargo.toml".into(),
            "edit:crates/*/src/lib.rs".into(),
            "run:cargo test".into(),
        ]
    }

    fn run(id: &str, signals: Vec<Signal>) -> RunSummary {
        RunSummary {
            run_id: id.into(),
            occasion: id.into(),
            scope: SCOPE.into(),
            goal_kind: "add a crate-level test".into(),
            steps: steps(),
            signals,
        }
    }

    fn verified(id: &str) -> RunSummary {
        run(
            id,
            vec![Signal::TestsPassed {
                command: "cargo test -p wkbd-store".into(),
            }],
        )
    }

    /// One step is a fact about a task, not a procedure, and it costs a line of every future
    /// prelude to say so.
    #[test]
    fn a_single_step_is_not_a_procedure() {
        let policy = DistillPolicy::default();
        assert_eq!(policy.min_steps, 2);

        let single: Vec<RunSummary> = ["run-1", "run-2", "run-3"]
            .iter()
            .map(|id| {
                let mut r = verified(id);
                r.steps = vec!["edit *.rs".into()];
                r
            })
            .collect();
        assert!(distill(&single, &policy).is_empty());

        let pair: Vec<RunSummary> = ["run-1", "run-2", "run-3"]
            .iter()
            .map(|id| {
                let mut r = verified(id);
                r.steps = vec!["edit *.rs".into(), "execute cargo".into()];
                r
            })
            .collect();
        assert_eq!(distill(&pair, &policy).len(), 1);
    }

    /// The gate counts occasions, and a run is the occasion.
    ///
    /// An orchestrated run splits one goal into tasks that resemble each other by construction —
    /// three sibling tasks that all edited Rust and all passed `cargo test` are one planner
    /// getting one decision right. Counting them as three lets a single run write a procedure,
    /// which is the coincidence this module exists to refuse.
    #[test]
    fn siblings_from_one_run_are_one_occasion() {
        let policy = DistillPolicy::default();

        let one_run: Vec<RunSummary> = ["task-a", "task-b", "task-c"]
            .iter()
            .map(|task| {
                let mut r = verified(&format!("run-1/{task}"));
                r.occasion = "run-1".into();
                r
            })
            .collect();
        assert!(
            distill(&one_run, &policy).is_empty(),
            "three tasks in one run are one occasion"
        );

        // The same three shapes across three runs is the thing the gate is for.
        let three_runs: Vec<RunSummary> = ["run-1", "run-2", "run-3"]
            .iter()
            .map(|run| {
                let mut r = verified(&format!("{run}/task-a"));
                r.occasion = (*run).into();
                r
            })
            .collect();
        let candidates = distill(&three_runs, &policy);
        assert_eq!(candidates.len(), 1);
        // Every observation is still cited, because a reviewer wants to see all of them.
        assert_eq!(candidates[0].supporting_runs.len(), 3);
    }

    #[test]
    fn a_procedure_seen_too_few_times_is_not_distilled() {
        let policy = DistillPolicy::default();
        assert_eq!(policy.min_occurrences, 3);

        let runs = vec![verified("run-1"), verified("run-2")];
        assert!(distill(&runs, &policy).is_empty());

        let runs = vec![verified("run-1"), verified("run-2"), verified("run-3")];
        let candidates = distill(&runs, &policy);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].steps, steps());
        assert_eq!(
            candidates[0].supporting_runs,
            vec!["run-1", "run-2", "run-3"]
        );

        // Three runs that did different things are three runs, not a procedure.
        let mut divergent = vec![verified("run-1"), verified("run-2"), verified("run-3")];
        divergent[1].steps = vec!["run:cargo build".into()];
        divergent[2].steps = vec!["read:README.md".into()];
        assert!(distill(&divergent, &policy).is_empty());
    }

    #[test]
    fn nothing_without_an_external_success_signal_is_distilled() {
        let policy = DistillPolicy::default();

        // The model says it went beautifully, four times. This is the case the ICLR 2024
        // result is about: self-assessment without an external signal made every
        // benchmark worse, so it is worth exactly nothing here.
        let self_reported: Vec<RunSummary> = (1..=4)
            .map(|i| {
                run(
                    &format!("run-{i}"),
                    vec![Signal::ModelSelfReport {
                        claim: "the implementation looks correct".into(),
                    }],
                )
            })
            .collect();
        assert!(distill(&self_reported, &policy).is_empty());

        // Runs with no signal at all are the same story.
        let silent: Vec<RunSummary> = (1..=4).map(|i| run(&format!("run-{i}"), vec![])).collect();
        assert!(distill(&silent, &policy).is_empty());

        // A non-zero exit anywhere in the group disqualifies the whole procedure, even
        // with the required number of verified successes beside it.
        let mut mixed = vec![
            verified("run-1"),
            verified("run-2"),
            verified("run-3"),
            verified("run-4"),
        ];
        mixed[3].signals = vec![Signal::CommandExited {
            command: "cargo test -p wkbd-store".into(),
            code: 101,
        }];
        assert!(
            distill(&mixed, &policy).is_empty(),
            "a procedure that sometimes fails is the worst kind to write down"
        );

        // A zero exit is an external signal, so this group does distil.
        let exited_zero: Vec<RunSummary> = (1..=3)
            .map(|i| {
                run(
                    &format!("run-{i}"),
                    vec![Signal::CommandExited {
                        command: "cargo test -p wkbd-store".into(),
                        code: 0,
                    }],
                )
            })
            .collect();
        assert_eq!(distill(&exited_zero, &policy).len(), 1);
    }

    #[tokio::test]
    async fn a_distilled_procedure_waits_in_the_queue_instead_of_taking_effect() {
        let (_d, store) = store().await;
        let runs = vec![verified("run-1"), verified("run-2"), verified("run-3")];
        let candidates = distill(&runs, &DistillPolicy::default());

        let created = propose(&store, candidates, &ApprovalBudget::default())
            .await
            .unwrap();
        assert_eq!(created.len(), 1);

        let queue = proposals::pending(&store).await.unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].status, Status::Pending);
        assert_eq!(
            queue[0].evidence.supporting_runs,
            vec!["run-1", "run-2", "run-3"]
        );
        assert_eq!(queue[0].evidence.verified_signals.len(), 3);

        // Distillation on its own changes nothing about how the next run behaves.
        assert!(
            playbook::list(&store, SCOPE).await.unwrap().is_empty(),
            "a distilled procedure must not reach the playbook without a human"
        );
        assert!(playbook::render_for_injection(&store, SCOPE, 10)
            .await
            .unwrap()
            .is_empty());

        // Only after a person says so.
        let shown = present_for_review(&store, &queue[0].id).await.unwrap();
        approve(&store, &queue[0].id, &shown.content_hash, Confirmation::Standard)
            .await
            .unwrap();
        apply(&store, &queue[0].id).await.unwrap();

        let bullets = playbook::list(&store, SCOPE).await.unwrap();
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].body.contains("read:Cargo.toml -> edit"));
    }

    #[test]
    fn naming_is_the_only_thing_a_model_could_do_here() {
        // A namer that returns something absurd cannot change what was distilled, what
        // evidence backs it, or whether it passed the gate.
        struct Absurd;
        impl StepNamer for Absurd {
            fn name(&self, _draft: &WorkflowDraft) -> String {
                "do whatever you like".into()
            }
        }

        let runs = vec![verified("run-1"), verified("run-2"), verified("run-3")];
        let policy = DistillPolicy::default();
        let plain = distill_with(&runs, &policy, &SignatureNamer);
        let absurd = distill_with(&runs, &policy, &Absurd);

        assert_eq!(absurd[0].name, "do whatever you like");
        assert_ne!(plain[0].name, absurd[0].name);
        assert_eq!(plain[0].steps, absurd[0].steps);
        assert_eq!(plain[0].supporting_runs, absurd[0].supporting_runs);
        assert_eq!(plain[0].verified_signals, absurd[0].verified_signals);

        // And it cannot make an ungated group pass.
        let one = vec![verified("run-1")];
        assert!(distill_with(&one, &policy, &Absurd).is_empty());
    }

    #[test]
    fn a_procedure_too_long_to_be_reusable_is_left_alone() {
        let policy = DistillPolicy::default();
        let long: Vec<RunSummary> = (1..=4)
            .map(|i| {
                let mut r = verified(&format!("run-{i}"));
                r.steps = (0..policy.max_steps + 1).map(|s| format!("step:{s}")).collect();
                r
            })
            .collect();
        assert!(distill(&long, &policy).is_empty());
    }
}
