//! What a finished task actually did, reduced to something that can repeat.
//!
//! [`wkbd_evolve::distill`] can find a procedure in a pile of run summaries and has been able to
//! since it was written. Nothing built the pile. This does.
//!
//! # The unit is a task, not a run
//!
//! A run is a goal, and two goals are never the same twice. A task inside a run is small, has an
//! acceptance check attached, and the acceptance check is decided by running a command — which
//! makes it the only place in this system that produces the external success signal the distiller
//! insists on. Distilling runs would mean grouping on something that never repeats, gated by a
//! signal that does not exist at that level.
//!
//! # Normalising a step is the whole problem
//!
//! The distiller groups on an exact sequence match, so the signature has to throw away everything
//! incidental or no two tasks will ever agree. An agent's tool call titles are prose it wrote —
//! "Read src/config.rs", "Check the loader" — and grouping on those makes every task unique, which
//! looks like a distiller that never fires rather than like a bad signature.
//!
//! So a step becomes its kind plus the coarsest useful description of its target: a file extension
//! for reads and edits, the program name for a command. `read *.rs`, `execute cargo`. Consecutive
//! identical steps collapse, because "read four files" and "read six files" are the same procedure
//! and counting them is how a signature stops matching itself.
//!
//! The risk in the other direction is real and worth stating: a signature this coarse can group two
//! tasks that did different things in the same shape. That is what the approval queue is for. The
//! proposal carries the runs it came from, so a reviewer can look.
//!
//! # Failures are collected too
//!
//! Not an oversight to fix later — the gate depends on them. One failure anywhere in a group
//! disqualifies the whole procedure, so a tracer that returned only successes would produce a
//! distiller that cheerfully writes down recipes known to fail, having been shown only the times
//! they worked.

use anyhow::Result;
use std::collections::BTreeSet;
use wkbd_evolve::distill::{RunSummary, Signal};
use wkbd_proto::{EventPayload, FileOp, RunEvent, ToolKind};
use wkbd_store::Store;

/// How many finished tasks are considered. A bound on work, not on quality: distillation runs at
/// the end of every run and reads the history each time, and a project with a thousand tasks
/// behind it must not make the last event of a run wait on all of them.
const MAX_TASKS: usize = 400;

/// One finished task, as the run's event stream has it.
///
/// Reconstructed from events rather than read from the `tasks` table, which looks like the
/// obvious source and is empty: nothing has ever written to it. The durable workflow checkpoints
/// into `step_outputs` and everything a reader needs is in the log, which is where the rest of
/// this system takes its answers from anyway.
struct FinishedTask {
    run_id: String,
    task_id: String,
    passed: bool,
    declared_paths: Vec<String>,
    verify_cmd: String,
}

/// Every finished task in this project, as summaries the distiller can group.
pub async fn traces_for_project(store: &Store, project_root: &str) -> Result<Vec<RunSummary>> {
    let tasks = finished_tasks(store, project_root).await?;
    let mut out = Vec::with_capacity(tasks.len());

    for task in tasks {
        let steps = match session_for(store, &task.run_id, &task.task_id).await? {
            Some(session) => steps_in_session(store, &session)?,
            // A task with no session did no work we can describe. Keeping it with empty steps
            // would put it in the "no steps" bucket the distiller already skips, so it is dropped
            // here where the reason is visible.
            None => continue,
        };
        if steps.is_empty() {
            continue;
        }

        let signal = if task.passed {
            Signal::TestsPassed { command: task.verify_cmd.clone() }
        } else {
            // The exit code is not recorded anywhere — acceptance keeps the verdict, not the
            // status. Any non-zero value carries the same meaning to the distiller, which asks
            // only whether this is a failure.
            Signal::CommandExited { command: task.verify_cmd.clone(), code: 1 }
        };

        out.push(RunSummary {
            // Namespaced, because two runs can and do use the same task ids, and the distiller
            // cites these as its supporting evidence. Three citations of "task-a" would leave a
            // reviewer unable to tell which runs to go and read.
            run_id: format!("{}/{}", task.run_id, task.task_id),
            // The run, not the task. Sibling tasks come from one planner acting once, so they are
            // one occasion however many of them there are — see `RunSummary::occasion`.
            occasion: task.run_id.clone(),
            scope: project_root.to_string(),
            goal_kind: goal_kind(&task.declared_paths),
            steps,
            signals: vec![signal],
        });
    }

    Ok(out)
}

async fn finished_tasks(store: &Store, project_root: &str) -> Result<Vec<FinishedTask>> {
    let mut out = Vec::new();
    for run_id in runs_of_project(store, project_root).await? {
        out.extend(tasks_in_run(store, &run_id)?);
        if out.len() >= MAX_TASKS {
            break;
        }
    }
    Ok(out)
}

async fn runs_of_project(store: &Store, project_root: &str) -> Result<Vec<String>> {
    let root = project_root.to_string();
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM runs WHERE project_root = ?1 ORDER BY created_ms DESC LIMIT 200",
            )?;
            let rows = stmt.query_map([&root], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

fn tasks_in_run(store: &Store, run_id: &str) -> Result<Vec<FinishedTask>> {
    let stream = wkbd_proto::run_stream_id(run_id);
    let (events, _) = store.read_since(Some(&stream), 0, 10_000)?;

    // The last plan wins. A run that was replanned executed the graph it ended with, and the
    // rejected attempt describes work that never happened.
    let mut planned: Vec<wkbd_proto::TaskSummary> = Vec::new();
    let mut verdicts: Vec<(String, bool)> = Vec::new();
    for event in &events {
        if let EventPayload::Run { run } = &event.payload {
            match run {
                RunEvent::Planned { tasks, .. } => planned = tasks.clone(),
                RunEvent::TaskVerified { task_id, passed, .. } => {
                    verdicts.retain(|(id, _)| id != task_id);
                    verdicts.push((task_id.clone(), *passed));
                }
                _ => {}
            }
        }
    }

    let mut out = Vec::new();
    for (task_id, passed) in verdicts {
        let Some(spec) = planned.iter().find(|t| t.id == task_id) else { continue };
        out.push(FinishedTask {
            run_id: run_id.to_string(),
            task_id,
            passed,
            declared_paths: spec.declared_paths.clone(),
            verify_cmd: if spec.verify_cmd.trim().is_empty() {
                "the acceptance check".to_string()
            } else {
                spec.verify_cmd.clone()
            },
        });
    }
    Ok(out)
}

/// The worker session that did one task.
///
/// Matched on the worktree path a worker is opened in, which ends in the run and the task. The
/// suffix rather than a stored id, because nothing records the pairing anywhere else — and it is
/// anchored on the separator so that a task called `a` cannot match a session for `beta`.
async fn session_for(store: &Store, run_id: &str, task_id: &str) -> Result<Option<String>> {
    let suffix = format!("%/{run_id}/{task_id}");
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions
                 WHERE project_root LIKE ?1 ORDER BY created_ms DESC LIMIT 1",
            )?;
            let mut rows = stmt.query([&suffix])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get::<_, String>(0)?)),
                None => Ok(None),
            }
        })
        .await
}

fn steps_in_session(store: &Store, session_id: &str) -> Result<Vec<String>> {
    let (events, _) = store.read_since(Some(session_id), 0, 10_000)?;

    let mut steps: Vec<String> = Vec::new();
    for event in &events {
        let signature = match &event.payload {
            EventPayload::ToolCallStarted { title, kind, .. } => step_signature(kind, title),

            // File access performed for the agent counts as a step, and leaving it out was the
            // same blind spot the transcript had before these were rendered: an agent that edits
            // through the protocol's file methods — the path this daemon encourages, because it
            // is the only one that is bounded and logged — emits no tool calls at all, so its
            // work would be invisible here and every one of its tasks would distil to nothing.
            //
            // Only the allowed ones. A refusal is something that did not happen, and a procedure
            // is a description of what to do.
            EventPayload::FileAccess { op, requested, allowed: true, .. } => {
                let verb = match op {
                    FileOp::Read => "read",
                    FileOp::Write => "write",
                };
                match extension_in(requested) {
                    Some(ext) => format!("{verb} *.{ext}"),
                    None => verb.to_string(),
                }
            }

            _ => continue,
        };

        // Consecutive duplicates collapse. Reading four files and reading six is the same
        // procedure, and keeping the count is how a signature stops matching itself.
        if steps.last().map(|s| s.as_str()) != Some(signature.as_str()) {
            steps.push(signature);
        }
    }
    Ok(steps)
}

/// A step's kind plus the coarsest description of what it acted on.
fn step_signature(kind: &ToolKind, title: &str) -> String {
    let verb = kind_word(kind);
    match kind {
        // What matters about a command is which program ran. The arguments are where the
        // specifics live, and specifics are what stop two runs from matching.
        ToolKind::Execute => match program_in(title) {
            Some(program) => format!("{verb} {program}"),
            None => verb,
        },
        ToolKind::Read | ToolKind::Edit | ToolKind::Delete | ToolKind::Move => {
            match extension_in(title) {
                Some(ext) => format!("{verb} *.{ext}"),
                None => verb,
            }
        }
        _ => verb,
    }
}

/// A kind an agent invented is kept as itself rather than folded into `other`. Two agents that
/// both emit the same unrecognised kind are doing the same thing, and collapsing them would put
/// every unmodelled tool in one bucket where their sequences would agree by accident.
fn kind_word(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read => "read".into(),
        ToolKind::Edit => "edit".into(),
        ToolKind::Delete => "delete".into(),
        ToolKind::Move => "move".into(),
        ToolKind::Search => "search".into(),
        ToolKind::Execute => "execute".into(),
        ToolKind::Think => "think".into(),
        ToolKind::Fetch => "fetch".into(),
        ToolKind::SwitchMode => "switch mode".into(),
        ToolKind::Other => "other".into(),
        ToolKind::Unknown(name) => format!("({name})"),
    }
}

/// The program a command title names, if one is recognisable.
///
/// Titles are prose an agent wrote, so this looks for the first word that could be a command
/// rather than parsing. `Run cargo test` gives `cargo`; a title with no such word gives nothing,
/// and the step degrades to its kind alone rather than to a guess.
fn program_in(title: &str) -> Option<String> {
    const PREAMBLE: [&str; 6] = ["run", "running", "execute", "the", "a", "then"];
    title
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_'))
        .find(|w| {
            !w.is_empty()
                && !PREAMBLE.contains(&w.to_ascii_lowercase().as_str())
                && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(|w| w.to_ascii_lowercase())
}

/// The extension of the first path-looking token in a title.
fn extension_in(title: &str) -> Option<String> {
    title
        .split_whitespace()
        .filter(|w| w.contains('.'))
        .filter_map(|w| {
            let cleaned = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/');
            cleaned.rsplit('.').next().filter(|e| {
                !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())
            })
        })
        .map(|e| e.to_ascii_lowercase())
        .next()
}

/// A coarse class, from the kinds of file the task said it would touch.
///
/// Extensions rather than anything read from the task's prose. Two tasks are only ever compared
/// within one class, so the class has to be something that recurs — and a title does not. The
/// declared paths are the most specific thing about a task that a planner writes deliberately.
fn goal_kind(declared_paths: &[String]) -> String {
    let exts: BTreeSet<String> = declared_paths
        .iter()
        .filter_map(|p| {
            std::path::Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
        })
        .collect();

    if exts.is_empty() {
        return "unclassified".to_string();
    }
    exts.into_iter().collect::<Vec<_>>().join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_becomes_its_program() {
        assert_eq!(step_signature(&ToolKind::Execute, "Run cargo test"), "execute cargo");
        assert_eq!(step_signature(&ToolKind::Execute, "running npm run build"), "execute npm");
        // Nothing recognisable, so the step keeps its kind rather than inventing a program.
        assert_eq!(step_signature(&ToolKind::Execute, "Run the thing"), "execute thing");
        assert_eq!(step_signature(&ToolKind::Execute, "…"), "execute");
    }

    #[test]
    fn a_file_becomes_its_extension() {
        assert_eq!(step_signature(&ToolKind::Read, "Read src/config.rs"), "read *.rs");
        assert_eq!(step_signature(&ToolKind::Edit, "Edit ui/src/App.tsx"), "edit *.tsx");
        assert_eq!(step_signature(&ToolKind::Read, "Read the manifest"), "read");
    }

    /// The point of normalising at all. Two tasks that did the same thing to different files
    /// have to produce the same signature or nothing ever repeats.
    #[test]
    fn two_tasks_doing_the_same_thing_to_different_files_agree() {
        let a = step_signature(&ToolKind::Edit, "Edit crates/wkbd-core/src/api.rs");
        let b = step_signature(&ToolKind::Edit, "Edit crates/wkbd-proto/src/event.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn a_class_comes_from_the_declared_paths() {
        assert_eq!(goal_kind(&["src/a.rs".into(), "src/b.rs".into()]), "rs");
        assert_eq!(goal_kind(&["src/a.rs".into(), "Cargo.toml".into()]), "rs+toml");
        assert_eq!(goal_kind(&["docs".into()]), "unclassified");
    }
}
