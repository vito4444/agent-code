//! What the system takes away from a finished run.
//!
//! Two things happen here and they are deliberately asymmetric.
//!
//! **Facts are written directly.** They are inferred statements about the world, they carry a
//! confidence and a provenance, and every one of them can be superseded by a later fact or retired
//! by decay. Nothing that reads them treats them as instructions.
//!
//! **Anything the system would tell itself becomes a proposal instead.** The playbook, routing
//! preferences beyond the arithmetic, distilled workflows — those are instructions, and a run's
//! transcript contains text from tools, files and possibly a hostile repository. Text that can
//! reach the model's instructions is the single most valuable thing for an injection to reach, so
//! the rule is that it goes into a queue with its evidence attached and takes effect only when a
//! person agrees.
//!
//! The asymmetry is the design. Making both a proposal would bury the person in fact approvals
//! until they stopped reading them, which is how an approval gate stops being one. Making both
//! automatic would put an attacker's text in the next session's instructions.

use anyhow::Result;
use wkbd_evolve::distill::DistillPolicy;
use wkbd_evolve::proposals::{self, ApprovalBudget, Evidence, NewProposal, ProposalPayload};

use wkbd_proto::{EventPayload, RunEvent};
use wkbd_store::Store;

/// Reads a finished run and records what can be learned from it.
///
/// Failures are logged, never propagated. This runs after the work is done and the candidate exists;
/// a run that succeeded must not be reported as failed because the thing that learns from it did.
pub async fn from_run(store: &Store, run_id: &str, project_root: &str) {
    if let Err(e) = extract_facts(store, run_id, project_root).await {
        tracing::warn!(run = %run_id, error = %e, "could not extract facts from a run");
    }
    if let Err(e) = propose_from_outcome(store, run_id, project_root).await {
        tracing::warn!(run = %run_id, error = %e, "could not raise proposals from a run");
    }
    if let Err(e) = distil_procedures(store, project_root).await {
        tracing::warn!(project = %project_root, error = %e, "could not distil procedures");
    }
}

/// Looks for a procedure in everything this project has finished, not just in this run.
///
/// Which is why it takes no run id. A procedure is by definition something that happened more than
/// once, so the unit of evidence is the project's history and this run is one more data point in
/// it. Running the whole search after every run rather than accumulating state means there is no
/// index to fall out of date, and the re-derivation is cheap enough at the scale a local workbench
/// reaches.
///
/// Re-deriving does mean the same procedure is found again after every subsequent run. It is
/// filed once: [`proposals::create`] treats the content hash as the identity of a decision, so the
/// second finding returns the first row instead of growing the queue.
async fn distil_procedures(store: &Store, project_root: &str) -> Result<()> {
    let traces = crate::trace::traces_for_project(store, project_root).await?;
    let candidates = wkbd_evolve::distill::distill_with(
        &traces,
        &DistillPolicy::default(),
        &crate::trace::ClassNamer,
    );
    if candidates.is_empty() {
        // Logged rather than returned early and silently, and the trace count is in it. Nothing
        // to distil and nothing to distil *from* look identical from outside, and the second is
        // a defect — the first version of this read a table that has never had a row written to
        // it, found no tasks, and reported exactly as much as a healthy quiet run does.
        tracing::info!(
            project = %project_root,
            traces = traces.len(),
            "nothing repeated often enough to distil"
        );
        return Ok(());
    }

    let created =
        wkbd_evolve::distill::propose(store, candidates, &ApprovalBudget::default()).await?;
    let fresh = created.iter().filter(|c| c.is_new()).count();
    tracing::info!(
        project = %project_root,
        found = created.len(),
        queued = fresh,
        "distilled procedures from repeated verified success"
    );
    Ok(())
}

/// Facts from the run's own events and from every session that took part in it.
///
/// Both, because they answer different questions. The run's events say which tasks passed and how
/// they were combined; the sessions' events say what the agents actually did — which commands were
/// run, which files were touched, what failed. A summary of only the first would record that a task
/// passed without recording anything reusable about how.
async fn extract_facts(store: &Store, run_id: &str, project_root: &str) -> Result<()> {
    let stream = wkbd_proto::run_stream_id(run_id);
    let (run_events, _) = store.read_since(Some(&stream), 0, 10_000)?;

    let mut all = run_events.clone();
    for session in sessions_in_run(store, run_id).await? {
        let (events, _) = store.read_since(Some(&session), 0, 10_000)?;
        all.extend(events);
    }

    let facts = wkbd_memory::extract::from_events(&all, Some(project_root));
    if facts.is_empty() {
        return Ok(());
    }

    let judge = wkbd_memory::facts::SamePredicateJudge;
    let mut recorded = 0;
    for mut fact in facts {
        // Provenance is not optional. A fact with no source cannot be re-examined when it turns out
        // to be wrong, and "where did the system get this idea" is the first question anyone asks
        // about a memory that misled them.
        fact.source_run = Some(run_id.to_string());
        // Observed from our own run — a test result, a command we executed — rather than something
        // the user said. That distinction is what keeps the decay and supersession machinery willing
        // to retire it, and what keeps it below user rules at injection time. Nothing extracted here
        // can ever be `User`; that vocabulary belongs to rules and the extractor cannot reach it.
        fact.source_trust = wkbd_memory::facts::SourceTrust::Internal;
        match wkbd_memory::facts::record(store, &judge, fact).await {
            Ok(_) => recorded += 1,
            Err(e) => tracing::debug!(error = %e, "a fact could not be recorded"),
        }
    }
    tracing::info!(run = %run_id, recorded, "recorded facts from a run");
    Ok(())
}

/// Which conversation streams belong to a run.
///
/// Found by where the session was rooted. A worker's project root is its worktree, which lives at
/// `<state>/worktrees/<run id>/<task id>`, so the run id is already in data we recorded and needs no
/// extra event or column to recover.
///
/// Matched on a path segment rather than a substring: `LIKE '%<id>%'` would also match a run id that
/// happens to appear inside a directory name a user chose, and attributing a stranger's session to
/// this run would attach one piece of work's behaviour to another's conclusions.
async fn sessions_in_run(store: &Store, run_id: &str) -> Result<Vec<String>> {
    let needle = format!("%/worktrees/{run_id}/%");
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions WHERE project_root LIKE ?1 ORDER BY created_ms",
            )?;
            let rows = stmt.query_map([&needle], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Raises a proposal when a run says something about how work should be done.
///
/// The evidence is the run. That is the whole point of the shape: a person looking at the queue can
/// see which run produced the suggestion and what happened in it, rather than being asked to agree
/// with an assertion that arrived from nowhere.
async fn propose_from_outcome(store: &Store, run_id: &str, project_root: &str) -> Result<()> {
    let stream = wkbd_proto::run_stream_id(run_id);
    let (events, _) = store.read_since(Some(&stream), 0, 10_000)?;

    let mut failures: Vec<(String, Vec<String>)> = Vec::new();
    let mut passed = 0;
    let mut replans = 0;
    for event in &events {
        if let EventPayload::Run { run } = &event.payload {
            match run {
                RunEvent::TaskVerified { task_id, passed: ok, missing_pass, .. } => {
                    if *ok {
                        passed += 1;
                    } else {
                        failures.push((task_id.clone(), missing_pass.clone()));
                    }
                }
                RunEvent::Replanning { .. } => replans += 1,
                _ => {}
            }
        }
    }

    // Only raised when there is something to say. A queue that fills up after every run stops being
    // read, and an approval gate nobody reads is not a gate.
    if failures.is_empty() && replans == 0 {
        return Ok(());
    }

    // Sentences, without a leading marker. The marker belongs to whoever renders the line, and
    // there are two of them: the approval queue puts each change in a list item, and the playbook
    // prefixes every bullet on its way into a prelude. A body that carried its own dash produced
    // "- - Task ..." in the text an agent reads and a bullet beside a dash in the queue.
    let mut lines = Vec::new();
    if replans > 0 {
        lines.push(format!(
            "When a task changes files outside its declared paths ({replans} did in this run), \
             declare the paths more broadly rather than splitting the task."
        ));
    }
    for (task, missing) in &failures {
        if !missing.is_empty() {
            lines.push(format!(
                "Task {task:?} did not make {} start passing. Check whether the assertion names a \
                 test that exists before planning around it.",
                missing.join(", ")
            ));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }

    let payload = ProposalPayload::Playbook {
        scope: project_root.to_string(),
        // `add` only. There is no operation that replaces a section, because the fastest route to a
        // playbook made of platitudes is letting a model rewrite one it wrote.
        deltas: lines
            .iter()
            .map(|l| {
                wkbd_evolve::PlaybookDelta::add(l.clone(), wkbd_evolve::SourceTrust::Internal)
            })
            .collect(),
    };

    let evidence = Evidence {
        supporting_runs: vec![run_id.to_string()],
        // Externally checkable, not the model's opinion of its own work: how many acceptance checks
        // passed, how many failed, how many tasks left their declared paths.
        verified_signals: lines.clone(),
        note: format!(
            "{passed} task(s) passed acceptance, {} failed, {replans} left their declared paths",
            failures.len()
        ),
    };

    let created = proposals::create(
        store,
        NewProposal::new(payload, evidence),
        &ApprovalBudget::default(),
    )
    .await?;
    tracing::info!(
        run = %run_id,
        project = %project_root,
        proposal = %created.proposal().id,
        "raised a proposal from a run"
    );
    Ok(())
}

/// Spawns the learning pass so it cannot delay the run reporting its result.
pub fn spawn(store: Store, run_id: String, project_root: String) {
    tokio::spawn(async move {
        from_run(&store, &run_id, &project_root).await;
    });
}


