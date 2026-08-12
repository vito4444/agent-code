//! Where the model is allowed to make decisions.
//!
//! Two entry points and no others: drafting a graph from a sentence, and redrafting one after a
//! deterministic predicate said the previous graph was wrong. Everything downstream — ordering,
//! dispatch, isolation, dependency integration, ownership checking, acceptance, merging — is
//! ordinary code.
//!
//! The reason for the boundary is not that models are unreliable. It is that a run assembled this
//! way can be replayed: every decision after the draft is a function of recorded inputs, so
//! reading the log tells you why the run did what it did. A run whose scheduling decisions came
//! from a model cannot be replayed at all, only re-sampled — which produces a different run that
//! happens to start from the same prompt, and that is not a debugging tool.
//!
//! A trait, and not because of some abstraction principle: the deterministic implementation is
//! what makes the orchestrator testable end to end without credentials, and a graph that arrives
//! as JSON is exactly as valid whether a model or a fixture produced it.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use wkbd_orch::{DraftGraph, GraphProblem};

#[async_trait]
pub trait Planner: Send + Sync {
    /// One sentence to a draft graph.
    async fn draft(&self, goal: &str, ctx: &PlanContext) -> Result<DraftGraph>;

    /// A rejected graph to a new draft.
    ///
    /// The problems are passed in structured form rather than as prose. A planner that is told
    /// "task b depends on task z, which does not exist" can fix that; one told "the graph was
    /// invalid" will resample and quite likely produce the same graph again.
    async fn redraft(
        &self,
        goal: &str,
        ctx: &PlanContext,
        previous: &DraftGraph,
        problems: &[String],
    ) -> Result<DraftGraph>;
}

pub struct PlanContext {
    pub project_root: String,
    /// What we can tell the planner about the repository. Deliberately small: a planner given the
    /// whole tree spends its attention on the tree rather than on the goal.
    pub repo_summary: String,
    /// Test identifiers that exist, when we could enumerate them. The planner is told to choose
    /// assertions from this list, because an assertion naming a test that does not exist makes a
    /// task that can never be accepted.
    pub known_tests: Vec<String>,
}

/// The prompt the drafting step sends.
///
/// Public and a plain function so the wording is reviewable and testable rather than buried in a
/// method. The constraints in it are the same ones validation enforces: a planner that is not told
/// the rules will break them, and every broken rule costs a round trip.
pub fn draft_prompt(goal: &str, ctx: &PlanContext) -> String {
    let tests = if ctx.known_tests.is_empty() {
        // Saying "there are none" invites the planner to invent identifiers, which then fail
        // validation. Saying we could not enumerate them is both true and actionable.
        "The test inventory could not be enumerated, so any identifier will be accepted. Prefer \
         identifiers that plausibly exist in this repository."
            .to_string()
    } else {
        format!(
            "Choose assertions only from these test identifiers:\n{}",
            ctx.known_tests
                .iter()
                .take(200)
                .map(|t| format!("  {t}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    format!(
        r#"Break this goal into a task graph. Reply with JSON only, no prose and no code fence.

GOAL: {goal}

PROJECT: {root}

{summary}

{tests}

Shape:
{{
  "goal": "restate the goal",
  "tasks": [
    {{
      "id": "short-slug",
      "title": "one line",
      "body": "what the agent doing this task should do, in full",
      "declared_paths": ["src/thing.rs"],
      "depends_on": [],
      "verify": {{
        "setup": [],
        "cmd": "the command that runs the tests",
        "must_pass": ["test::that::should::start::passing"],
        "must_still_pass": ["test::that::must::not::break"],
        "immutable_paths": ["tests/**"]
      }}
    }}
  ]
}}

Rules, each of which is checked and will send this back to you if broken:

- Every task needs at least one entry in must_pass or must_still_pass. A task whose acceptance is
  "the command exits zero" is not an acceptance criterion, it is a command.
- Every id is unique. Every entry in depends_on names a task in this graph. No cycles. No task
  depends on itself.
- In a graph with any edges, do not leave a task with no dependencies and no dependents. If it is
  genuinely independent say so by making it the only such task; otherwise connect it.
- declared_paths is what the task will touch. It is used to order work and afterwards to check
  whether you were right, and a task that touches something outside it fails. Two tasks that
  declare the same path will be made to run in sequence rather than at once.
- immutable_paths must cover the tests and anything that decides what passing means. A task that
  can edit its own acceptance test has no acceptance test.
- Prefer fewer, larger tasks over many small ones. Each task costs a worktree, an agent session
  and a test run.
"#,
        goal = goal,
        root = ctx.project_root,
        summary = ctx.repo_summary,
        tests = tests,
    )
}

pub fn redraft_prompt(
    goal: &str,
    ctx: &PlanContext,
    previous: &DraftGraph,
    problems: &[String],
) -> String {
    format!(
        r#"{base}

Your previous graph was rejected. Here it is:

{previous}

These are the problems found. Fix all of them; a graph that fixes some will be rejected again.

{problems}
"#,
        base = draft_prompt(goal, ctx),
        previous = serde_json::to_string_pretty(previous).unwrap_or_default(),
        problems = problems.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"),
    )
}

/// Pulls a graph out of whatever the planner actually said.
///
/// Liberal on purpose. Every model wraps JSON in something at least sometimes — a fence, a
/// sentence of preamble, a trailing explanation — and rejecting the reply for that costs a round
/// trip to fix punctuation rather than substance. What is *not* liberal is the shape once parsed:
/// a graph missing a field is rejected, because filling in a default there would invent an
/// acceptance criterion.
pub fn extract_graph(reply: &str) -> Result<DraftGraph> {
    let trimmed = reply.trim();

    if let Ok(g) = serde_json::from_str::<DraftGraph>(trimmed) {
        return Ok(g);
    }

    // A fenced block, with or without a language tag.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let body = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = body.find("```") {
            if let Ok(g) = serde_json::from_str::<DraftGraph>(body[..end].trim()) {
                return Ok(g);
            }
        }
    }

    // The outermost brace pair. Scanning for balance rather than taking the first `{` and last `}`
    // because a reply can contain more than one object, and the naive slice joins two of them into
    // something that parses as neither.
    if let Some(candidate) = outermost_object(trimmed) {
        return serde_json::from_str::<DraftGraph>(candidate)
            .with_context(|| format!("the planner's JSON does not match the expected shape: {candidate:.400}"));
    }

    Err(anyhow!(
        "the planner replied with no JSON object: {:.400}",
        trimmed
    ))
}

fn outermost_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Renders validation problems the way the planner will be shown them.
pub fn describe_problems(problems: &[GraphProblem]) -> Vec<String> {
    problems.iter().map(describe_problem).collect()
}

fn describe_problem(p: &GraphProblem) -> String {
    match p {
        GraphProblem::Empty => "the graph has no tasks".to_string(),
        GraphProblem::DuplicateTaskId { id } => format!("two tasks share the id {id:?}"),
        GraphProblem::UnknownDependency { task, depends_on } => {
            format!("task {task:?} depends on {depends_on:?}, which is not a task in this graph")
        }
        GraphProblem::SelfDependency { task } => format!("task {task:?} depends on itself"),
        GraphProblem::Cycle { cycle } => {
            format!("these tasks depend on each other in a cycle: {}", cycle.join(" -> "))
        }
        GraphProblem::Orphan { task } => format!(
            "task {task:?} has no dependencies and nothing depends on it, in a graph that has \
             edges; connect it or explain why it is independent"
        ),
        GraphProblem::UndecidableVerify { task, reason } => {
            format!("task {task:?} cannot be accepted or rejected: {reason}")
        }
        GraphProblem::UnknownTest { task, test } => format!(
            "task {task:?} names the test {test:?}, which does not exist in this repository, so \
             the task could never be accepted"
        ),
        GraphProblem::EmptyTitle { task } => format!("task {task:?} has no title"),
        GraphProblem::ImmutablePathAlsoDeclared { task, path } => format!(
            "task {task:?} declares {path:?} as a path it will change and also as immutable"
        ),
    }
}

/// Returns a fixed graph, ignoring the goal.
///
/// Not a mock in the sense of "stands in for the real thing": a graph is a graph, and the entire
/// orchestrator downstream of this point behaves identically whether a model or a fixture produced
/// it. That is the property being exercised, and it is why the end-to-end test can run a real
/// multi-task graph against a real repository with no credentials anywhere.
pub struct FixedPlanner {
    pub graphs: Vec<DraftGraph>,
    calls: std::sync::atomic::AtomicUsize,
}

impl FixedPlanner {
    /// Later graphs are returned by successive redrafts, so a test can script "the first graph is
    /// rejected, the second is fine".
    pub fn new(graphs: Vec<DraftGraph>) -> Self {
        Self { graphs, calls: std::sync::atomic::AtomicUsize::new(0) }
    }

    fn next(&self) -> Result<DraftGraph> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.graphs
            .get(n)
            .cloned()
            .ok_or_else(|| anyhow!("the fixed planner ran out of graphs after {n} calls"))
    }
}

#[async_trait]
impl Planner for FixedPlanner {
    async fn draft(&self, _goal: &str, _ctx: &PlanContext) -> Result<DraftGraph> {
        self.next()
    }
    async fn redraft(
        &self,
        _goal: &str,
        _ctx: &PlanContext,
        _previous: &DraftGraph,
        _problems: &[String],
    ) -> Result<DraftGraph> {
        self.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPH: &str = r#"{"goal":"g","tasks":[{"id":"a","title":"t","body":"b","verify":{"cmd":"cargo test","must_pass":["x"]}}]}"#;

    #[test]
    fn takes_bare_json() {
        assert_eq!(extract_graph(GRAPH).unwrap().tasks.len(), 1);
    }

    #[test]
    fn takes_a_fenced_block() {
        let reply = format!("Here you go:\n```json\n{GRAPH}\n```\nHope that helps.");
        assert_eq!(extract_graph(&reply).unwrap().tasks.len(), 1);
    }

    #[test]
    fn takes_an_unfenced_object_with_prose_around_it() {
        let reply = format!("I think this works: {GRAPH} — let me know.");
        assert_eq!(extract_graph(&reply).unwrap().tasks.len(), 1);
    }

    /// The naive "first brace to last brace" slice joins two objects into something that parses as
    /// neither, and the resulting error blames the planner for a bug in the extractor.
    #[test]
    fn takes_the_first_complete_object_not_the_span_between_the_outer_braces() {
        let reply = format!("{GRAPH}\n\nand an unrelated note: {{\"aside\": 1}}");
        let g = extract_graph(&reply).unwrap();
        assert_eq!(g.tasks.len(), 1);
    }

    /// Braces inside a string must not be counted, or a task body containing one truncates the
    /// object at the wrong place.
    #[test]
    fn braces_inside_strings_do_not_end_the_object() {
        let reply = r#"{"goal":"use HashMap<K, V> { }","tasks":[{"id":"a","title":"t","body":"write fn main() { }","verify":{"cmd":"cargo test","must_pass":["x"]}}]}"#;
        let g = extract_graph(reply).unwrap();
        assert_eq!(g.goal, "use HashMap<K, V> { }");
        assert_eq!(g.tasks[0].body, "write fn main() { }");
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let reply = r#"{"goal":"say \"hi\" }","tasks":[{"id":"a","title":"t","body":"b","verify":{"cmd":"c","must_pass":["x"]}}]}"#;
        assert_eq!(extract_graph(reply).unwrap().goal, "say \"hi\" }");
    }

    /// Being liberal about the wrapping does not extend to being liberal about the contents. A
    /// missing acceptance criterion has to be an error, because the alternative is defaulting one
    /// in, and a task accepted by a criterion nobody wrote is worse than a rejected draft.
    #[test]
    fn a_task_with_no_verify_block_is_rejected_rather_than_defaulted() {
        let reply = r#"{"goal":"g","tasks":[{"id":"a","title":"t","body":"b"}]}"#;
        assert!(extract_graph(reply).is_err());
    }

    #[test]
    fn prose_with_no_json_is_an_error_that_quotes_the_reply() {
        let err = extract_graph("I would rather not.").unwrap_err().to_string();
        assert!(err.contains("I would rather not"), "{err}");
    }

    #[test]
    fn the_prompt_states_the_rules_that_validation_enforces() {
        let ctx = PlanContext {
            project_root: "/r".into(),
            repo_summary: "s".into(),
            known_tests: vec!["a::b".into()],
        };
        let p = draft_prompt("do a thing", &ctx);
        // Each of these corresponds to a GraphProblem variant. A rule validation checks and the
        // prompt omits is a guaranteed round trip.
        for expected in ["must_pass", "cycles", "unique", "immutable_paths", "declared_paths"] {
            assert!(
                p.to_lowercase().contains(&expected.to_lowercase()),
                "the prompt does not mention {expected}, which validation enforces"
            );
        }
        assert!(p.contains("a::b"), "known tests must be offered to the planner");
    }

    #[test]
    fn an_empty_inventory_says_so_rather_than_offering_an_empty_list() {
        let ctx = PlanContext {
            project_root: "/r".into(),
            repo_summary: "s".into(),
            known_tests: vec![],
        };
        let p = draft_prompt("g", &ctx);
        assert!(p.contains("could not be enumerated"));
    }

    #[test]
    fn problems_name_the_task_and_the_field() {
        let described = describe_problems(&[
            GraphProblem::UnknownDependency { task: "b".into(), depends_on: "z".into() },
            GraphProblem::UnknownTest { task: "a".into(), test: "nope::x".into() },
        ]);
        assert!(described[0].contains("\"b\"") && described[0].contains("\"z\""));
        assert!(described[1].contains("nope::x"));
    }

    #[test]
    fn a_redraft_prompt_carries_the_previous_graph_and_every_problem() {
        let ctx = PlanContext {
            project_root: "/r".into(),
            repo_summary: "s".into(),
            known_tests: vec![],
        };
        let previous = extract_graph(GRAPH).unwrap();
        let p = redraft_prompt("g", &ctx, &previous, &["first".into(), "second".into()]);
        assert!(p.contains("\"id\": \"a\""), "the previous graph must be shown verbatim");
        assert!(p.contains("first") && p.contains("second"));
    }
}

/// Asks an agent for the graph.
///
/// The real implementation, and it needs no credential of its own: whatever agent the user already
/// configured is the planner. That matters more than it sounds — a planner behind a separate API key
/// is a second thing to configure and a second thing to be rate-limited by, and the agent already
/// has the repository in front of it.
///
/// The reply is read back out of the event log rather than returned by the turn. The log is where it
/// ends up anyway, and reading it there means the planner sees exactly what a human reading the
/// transcript sees — so a planner failure can be diagnosed from the same place as everything else.
pub struct AgentPlanner {
    pub state: std::sync::Arc<crate::state::AppState>,
    pub agent_id: String,
    pub project_root: String,
}

impl AgentPlanner {
    async fn ask(&self, prompt: &str) -> Result<DraftGraph> {
        let session = self
            .state
            .open_session(
                &self.agent_id,
                &self.project_root,
                wkbd_agent::SessionPurpose::NewChat,
            )
            .await
            .context("opening a planner session")?;

        let before = self
            .state
            .store
            .read_since(Some(&session.handle.local_id), 0, 1)
            .map(|(_, latest)| latest)
            .unwrap_or(0);

        let state = self.state.clone();
        let publisher = state.clone();
        let ctx = crate::runner::TurnContext {
            store: state.store.clone(),
            publish: std::sync::Arc::new(move |events: &[wkbd_proto::Event]| {
                publisher.publish(events)
            }),
            session_local_id: session.handle.local_id.clone(),
            handle: session.handle.clone(),
            permissions: state.permissions.clone(),
            project_root: session.project_root.clone(),
            // Planning is unattended in the same way a worker is. A planner mostly reads, and what
            // constrains it is the path guard rooted at the project.
            ask_user: std::sync::Arc::new(crate::runner::AutoAllow),
            guard: session.guard.clone(),
        };

        {
            let mut inbox = session.inbox.lock().await;
            crate::runner::run_turn(&ctx, prompt, &mut inbox).await?;
        }

        let (events, _) = self
            .state
            .store
            .read_since(Some(&session.handle.local_id), before, 5000)
            .context("reading the planner's reply")?;

        // Only the answer text. Thoughts are excluded on purpose: a planner that reasons out loud
        // about JSON will have braces in its reasoning, and including them makes the extractor pick
        // up a fragment of the thinking instead of the answer. The kind lives on the segment's
        // opening event rather than on each chunk, so the answer segments have to be identified
        // first — filtering chunks alone would take the thoughts too.
        let mut answer_segments = std::collections::HashSet::new();
        for event in &events {
            if let wkbd_proto::EventPayload::SegmentStarted { segment, kind } = &event.payload {
                if *kind == wkbd_proto::SegmentKind::Message {
                    answer_segments.insert(segment.raw.clone());
                }
            }
        }
        let mut reply = String::new();
        for event in &events {
            if let wkbd_proto::EventPayload::SegmentChunk { segment, text } = &event.payload {
                if answer_segments.contains(&segment.raw) {
                    reply.push_str(text);
                }
            }
        }

        if reply.trim().is_empty() {
            return Err(anyhow!(
                "the planner produced no answer text; it emitted {} events",
                events.len()
            ));
        }

        extract_graph(&reply)
    }
}

#[async_trait]
impl Planner for AgentPlanner {
    async fn draft(&self, goal: &str, ctx: &PlanContext) -> Result<DraftGraph> {
        self.ask(&draft_prompt(goal, ctx)).await
    }

    async fn redraft(
        &self,
        goal: &str,
        ctx: &PlanContext,
        previous: &DraftGraph,
        problems: &[String],
    ) -> Result<DraftGraph> {
        self.ask(&redraft_prompt(goal, ctx, previous, problems)).await
    }
}
