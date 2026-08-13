//! The task graph, and the validation a model's output has to survive.
//!
//! A model produces a *draft*. What it produces is a set of nodes and edges plus, for each
//! node, an acceptance specification. Everything after that — ordering, dispatch, retry,
//! verification, merge — is ordinary code.
//!
//! Two things follow from that split.
//!
//! **The model states rules, not order.** It says "B needs A's output"; the sequence is a
//! topological sort. Asking for a sequence duplicates information that is already in the edges
//! and has to be corrected every time the edges change.
//!
//! **Validation is deterministic and rejects rather than repairs.** A draft with a cycle, a
//! dangling dependency, or an acceptance test that does not exist goes back to the model with
//! a structured error. Repairing it here would mean this code inventing task semantics, and
//! the resulting graph would be neither what the model planned nor something anyone reviewed.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Acceptance criteria, expressed so a machine can check them.
///
/// Deliberately not a prose "test strategy" field. Every existing tool in this space stores
/// acceptance as free text, which is why they all stop at human review: there is nothing to
/// execute. The two assertion lists mirror the structure used by the standard benchmark for
/// this kind of work — tests that must start passing, and tests that must not stop passing —
/// because a change that fixes one thing and breaks two is not an improvement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifySpec {
    /// Commands to run before the test command. Failure here is an infrastructure failure,
    /// reported separately from a test failure, because the two call for different responses.
    #[serde(default)]
    pub setup: Vec<String>,
    /// The command that runs the tests.
    pub cmd: String,
    /// Test identifiers that must pass after the change and are expected to fail before it.
    #[serde(default)]
    pub must_pass: Vec<String>,
    /// Test identifiers that were passing before and must still pass.
    #[serde(default)]
    pub must_still_pass: Vec<String>,
    /// Globs the agent is not allowed to modify: the tests themselves, the test runner
    /// configuration, anything that decides what counts as passing.
    ///
    /// Not advisory. Audits of this exact setup found frontier models exploiting writable
    /// tests the large majority of the time, and around a tenth of lines of a conftest-style
    /// hook is enough to report every test as passed while changing nothing.
    #[serde(default)]
    pub immutable_paths: Vec<String>,
}

impl VerifySpec {
    /// Whether this specification can actually decide anything.
    ///
    /// A spec with no assertions is not an acceptance test; it is a command that exits zero.
    pub fn is_decidable(&self) -> bool {
        !self.cmd.trim().is_empty()
            && !(self.must_pass.is_empty() && self.must_still_pass.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftTask {
    pub id: String,
    pub title: String,
    pub body: String,
    /// Files the model believes this task will touch.
    ///
    /// A scheduling heuristic and nothing more. Measured on this machine: two branches
    /// touching disjoint paths can still fail to merge, because a directory rename split
    /// conflicts without any individual file conflicting. So this is used to order work and to
    /// check afterwards whether the model was right, never to conclude that two tasks are safe
    /// to combine.
    #[serde(default)]
    pub declared_paths: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub verify: VerifySpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftGraph {
    pub goal: String,
    pub tasks: Vec<DraftTask>,
}

/// Why a draft was rejected.
///
/// Structured rather than a string, because it goes back to the model as input and the model
/// needs to know which task and which field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum GraphProblem {
    Empty,
    DuplicateTaskId { id: String },
    UnknownDependency { task: String, depends_on: String },
    SelfDependency { task: String },
    Cycle { cycle: Vec<String> },
    /// A task nothing depends on and which depends on nothing, in a graph that has edges.
    /// Usually a sign the model forgot to connect it rather than that it is genuinely
    /// independent, so it is surfaced rather than silently scheduled first.
    Orphan { task: String },
    UndecidableVerify { task: String, reason: String },
    /// An assertion naming a test that does not exist in the repository.
    UnknownTest { task: String, test: String },
    EmptyTitle { task: String },
    ImmutablePathAlsoDeclared { task: String, path: String },
}

#[derive(Debug, Clone)]
pub struct ValidatedGraph {
    pub goal: String,
    pub tasks: Vec<DraftTask>,
    /// Task ids in an order where every dependency precedes its dependent.
    pub topological_order: Vec<String>,
    /// Groups of tasks that can run at the same time, derived from the edges.
    pub waves: Vec<Vec<String>>,
}

/// Decides whether a test identifier exists.
///
/// A trait because the answer is language- and runner-specific, and because tests need to
/// exercise the validation without a repository.
pub trait TestInventory {
    fn contains(&self, test_id: &str) -> bool;
}

/// Accepts every identifier. Used when no inventory is available, and it degrades the check
/// rather than skipping validation entirely.
pub struct AnyTest;

impl TestInventory for AnyTest {
    fn contains(&self, _test_id: &str) -> bool {
        true
    }
}

/// Validates a draft.
///
/// Returns every problem found rather than the first, so one round trip to the model can fix
/// all of them.
pub fn validate(
    draft: &DraftGraph,
    inventory: &dyn TestInventory,
) -> Result<ValidatedGraph, Vec<GraphProblem>> {
    let mut problems = Vec::new();

    if draft.tasks.is_empty() {
        return Err(vec![GraphProblem::Empty]);
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for task in &draft.tasks {
        if !seen.insert(&task.id) {
            problems.push(GraphProblem::DuplicateTaskId { id: task.id.clone() });
        }
        if task.title.trim().is_empty() {
            problems.push(GraphProblem::EmptyTitle { task: task.id.clone() });
        }
        if !task.verify.is_decidable() {
            problems.push(GraphProblem::UndecidableVerify {
                task: task.id.clone(),
                reason: if task.verify.cmd.trim().is_empty() {
                    "no verification command".into()
                } else {
                    "no assertions, so the command can only report that it ran".into()
                },
            });
        }
        // Only the regression set is checked against the inventory, and the asymmetry is the
        // whole point of having two fields.
        //
        // `must_pass` names what has to pass *after* the change and is expected to fail before
        // it, so a task that writes a new test names one that does not exist yet. That is the
        // ordinary case, not an error. Checking it here would reject exactly the tasks doing
        // test-driven work — and it did check it, harmlessly, for as long as the only inventory
        // in the tree accepted everything. The first real inventory would have turned a dormant
        // mistake into a validator that refuses correct plans.
        //
        // `must_still_pass` is the opposite claim: it names tests that were passing already. One
        // that does not exist is a planner inventing a regression guard, which is worth catching
        // before a worker spends a turn on it.
        for test in &task.verify.must_still_pass {
            if !inventory.contains(test) {
                problems.push(GraphProblem::UnknownTest {
                    task: task.id.clone(),
                    test: test.clone(),
                });
            }
        }
        // A task that both claims a path and declares it immutable is contradictory, and the
        // contradiction would be resolved at verification time by the anti-tamper layer
        // reverting the agent's own work.
        for immutable in &task.verify.immutable_paths {
            if task.declared_paths.iter().any(|p| p == immutable) {
                problems.push(GraphProblem::ImmutablePathAlsoDeclared {
                    task: task.id.clone(),
                    path: immutable.clone(),
                });
            }
        }
    }

    let ids: HashSet<&str> = draft.tasks.iter().map(|t| t.id.as_str()).collect();
    for task in &draft.tasks {
        for dep in &task.depends_on {
            if dep == &task.id {
                problems.push(GraphProblem::SelfDependency { task: task.id.clone() });
            } else if !ids.contains(dep.as_str()) {
                problems.push(GraphProblem::UnknownDependency {
                    task: task.id.clone(),
                    depends_on: dep.clone(),
                });
            }
        }
    }

    let has_edges = draft.tasks.iter().any(|t| !t.depends_on.is_empty());
    if has_edges && draft.tasks.len() > 1 {
        let depended_on: HashSet<&str> =
            draft.tasks.iter().flat_map(|t| t.depends_on.iter().map(|d| d.as_str())).collect();
        for task in &draft.tasks {
            if task.depends_on.is_empty() && !depended_on.contains(task.id.as_str()) {
                problems.push(GraphProblem::Orphan { task: task.id.clone() });
            }
        }
    }

    // Cycle detection runs even when there are unknown dependencies, ignoring the unknown
    // edges, so one round trip reports both classes of problem.
    if let Some(cycle) = find_cycle(&draft.tasks, &ids) {
        problems.push(GraphProblem::Cycle { cycle });
    }

    if !problems.is_empty() {
        return Err(problems);
    }

    let order = topological_order(&draft.tasks).expect("validated graph is acyclic");
    let waves = waves(&draft.tasks);

    Ok(ValidatedGraph {
        goal: draft.goal.clone(),
        tasks: draft.tasks.clone(),
        topological_order: order,
        waves,
    })
}

fn find_cycle(tasks: &[DraftTask], known: &HashSet<&str>) -> Option<Vec<String>> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        let entry = adjacency.entry(task.id.as_str()).or_default();
        for dep in &task.depends_on {
            if known.contains(dep.as_str()) && dep != &task.id {
                entry.push(dep.as_str());
            }
        }
    }

    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<&str> = Vec::new();

    fn visit<'a>(
        node: &'a str,
        adjacency: &HashMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match state.get(node) {
            Some(1) => {
                let start = stack.iter().position(|n| *n == node).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(node.to_string());
                return Some(cycle);
            }
            Some(2) => return None,
            _ => {}
        }
        state.insert(node, 1);
        stack.push(node);
        for next in adjacency.get(node).into_iter().flatten() {
            if let Some(cycle) = visit(next, adjacency, state, stack) {
                return Some(cycle);
            }
        }
        stack.pop();
        state.insert(node, 2);
        None
    }

    let mut nodes: Vec<&str> = adjacency.keys().copied().collect();
    nodes.sort_unstable();
    for node in nodes {
        if let Some(cycle) = visit(node, &adjacency, &mut state, &mut stack) {
            return Some(cycle);
        }
    }
    None
}

/// Kahn's algorithm, with ties broken by id so the order is reproducible.
///
/// Reproducibility matters more than it looks: a run that is replayed has to schedule work in
/// the same order, or the replay is a different run.
pub fn topological_order(tasks: &[DraftTask]) -> Option<Vec<String>> {
    let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for task in tasks {
        indegree.entry(task.id.as_str()).or_insert(0);
        for dep in &task.depends_on {
            *indegree.entry(task.id.as_str()).or_insert(0) += 1;
            dependents.entry(dep.as_str()).or_default().push(task.id.as_str());
        }
    }

    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();

    let mut order = Vec::new();
    while let Some(&next) = ready.iter().next() {
        ready.remove(next);
        order.push(next.to_string());
        for dependent in dependents.get(next).into_iter().flatten() {
            if let Some(d) = indegree.get_mut(*dependent) {
                *d -= 1;
                if *d == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }

    if order.len() == indegree.len() {
        Some(order)
    } else {
        None
    }
}

/// Groups tasks into waves: everything in one wave has all its dependencies satisfied by
/// earlier waves, so a wave can be dispatched in parallel.
pub fn waves(tasks: &[DraftTask]) -> Vec<Vec<String>> {
    let mut depth: HashMap<&str, usize> = HashMap::new();
    let order = match topological_order(tasks) {
        Some(o) => o,
        None => return Vec::new(),
    };
    let by_id: HashMap<&str, &DraftTask> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    for id in &order {
        let task = by_id[id.as_str()];
        let d = task
            .depends_on
            .iter()
            .filter_map(|dep| depth.get(dep.as_str()))
            .map(|d| d + 1)
            .max()
            .unwrap_or(0);
        depth.insert(task.id.as_str(), d);
    }

    let max_depth = depth.values().copied().max().unwrap_or(0);
    let mut out: Vec<Vec<String>> = vec![Vec::new(); max_depth + 1];
    for id in &order {
        out[depth[id.as_str()]].push(id.clone());
    }
    out
}
