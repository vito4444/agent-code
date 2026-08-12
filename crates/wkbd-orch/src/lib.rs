//! Orchestration.
//!
//! One sentence becomes a task graph, the graph becomes isolated parallel work, the work is
//! accepted or rejected by a machine-checkable criterion, and the accepted results are combined.
//!
//! The model appears at exactly two points: drafting the graph, and replanning when a
//! deterministic predicate says the graph was wrong. Everything else — validation, ordering,
//! dispatch, isolation, dependency integration, ownership checking, acceptance, merging — is
//! ordinary code with a durable checkpoint after each step. The reason is not purity. It is that
//! a run assembled this way can be read afterwards and replayed exactly, whereas a run whose
//! decisions came from a model can only be re-sampled, which is a different run that happens to
//! start from the same prompt.

pub mod durable;
pub mod graph;
pub mod schedule;
pub mod verify;

pub use durable::{unfinished_runs, RetryPolicy, RunStatus, Workflow};
pub use graph::{
    topological_order, validate, waves, DraftGraph, DraftTask, GraphProblem, TestInventory,
    ValidatedGraph, VerifySpec,
};
pub use schedule::{
    check_ownership, declared_path_overlaps, drain_merge_queue, prepare_workspace,
    MergeQueueOutcome, PrepareError, QueueEntry, ReplanTrigger, TaskWorkspace,
};
pub use verify::{
    hash_patch, CargoTestParser, ResultParser, VerifyOutcome, VerifyReport, Verifier,
};

#[cfg(test)]
mod tests;
