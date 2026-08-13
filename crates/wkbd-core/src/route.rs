//! Which agent gets a task.
//!
//! The first of the three learning loops, and the smallest claim of the three. Routing is a cost
//! control: the published comparison of routing methods found that most collapse to similar quality
//! and that commercial routers fail to beat simple baselines, so the objective here is the cheapest
//! arm that still clears a quality floor, not a better answer. Nothing in this module makes the work
//! better; it can only stop the expensive agent doing work a cheap one finishes correctly.
//!
//! ## What the loop is fed
//!
//! The acceptance check, and only the acceptance check. That is the whole reason this loop is worth
//! turning at all: the strong results for learning from experience rest on having a verifiable
//! signal, and where there is none the reported benefit collapses to roughly nothing. A task that
//! passed its own named assertions is a 1; one that did not is a 0. No model grades anything, and no
//! partial credit is invented for a task that failed in an interesting way.
//!
//! ## What it cannot learn yet, and why that is stated rather than papered over
//!
//! An agent is a command line. The daemon is told nothing about what that command line costs, so
//! unless somebody declares prices with `--agent-cost` every arm costs the same, the price penalty
//! multiplies by a constant, and the objective degenerates to "the arm most likely to pass". That is
//! a real limit rather than a bug: inventing prices would produce a router that is confidently
//! optimising something nobody measured.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use wkbd_evolve::routing::{ArmSpec, Decision, Features, Router, RouterConfig};
use wkbd_orch::DraftTask;
use wkbd_store::Store;

/// The role a routing decision is made under.
///
/// Roles are separate bandits. An agent that is good at writing code is not thereby good at breaking
/// a goal into tasks, and pooling both into one estimate would let evidence from the easier job move
/// the choice on the harder one.
pub const WORKER: &str = "worker";

pub struct Routing {
    router: Mutex<Router>,
    /// Empty when only one agent can do the job. A bandit over one arm is arithmetic with a fixed
    /// answer, and recording decisions for it fills the table with rows nothing can ever learn from.
    arms: Vec<ArmSpec>,
}

impl Routing {
    /// Builds the router over the agents that can act as workers.
    ///
    /// `costs` is what somebody declared, not what anything measured. Absent means 1.0, which makes
    /// the price half of the objective inert — see the module docs.
    pub async fn new(
        store: Store,
        agent_ids: &[String],
        costs: &BTreeMap<String, f64>,
    ) -> Result<Arc<Routing>> {
        let arms: Vec<ArmSpec> = agent_ids
            .iter()
            .map(|id| ArmSpec::new(id.clone(), costs.get(id).copied().unwrap_or(1.0)))
            .collect();

        let mut router = Router::new(store, RouterConfig::default());
        if arms.len() > 1 {
            router
                .register_role(WORKER, arms.clone(), &BTreeMap::new())
                .await?;
        }

        Ok(Arc::new(Routing {
            router: Mutex::new(router),
            arms: if arms.len() > 1 { arms } else { Vec::new() },
        }))
    }

    pub fn is_routing(&self) -> bool {
        !self.arms.is_empty()
    }

    /// Picks the agent for one task.
    ///
    /// `None` when there is nothing to pick between, and the caller falls back to the configured
    /// worker. Returning a decision anyway would record a choice that was never made.
    pub async fn choose(&self, task: &DraftTask, attempt: u32) -> Option<Decision> {
        if !self.is_routing() {
            return None;
        }
        let features = features_for(task, attempt);
        match self.router.lock().await.choose(WORKER, features).await {
            Ok(d) => Some(d),
            Err(e) => {
                // A router that will not answer must not stop the work. The caller falls back to the
                // configured worker, which is what would have happened with no router at all.
                tracing::warn!(error = %e, "could not route a task; using the configured worker");
                None
            }
        }
    }

    /// Folds in what happened.
    ///
    /// Called once per decision. A second call for the same decision is ignored by the router rather
    /// than doubling the weight of one observation, so a retrying caller cannot move the estimate by
    /// retrying.
    pub async fn reward(&self, decision_id: &str, passed: bool, cost: f64) {
        // Binary, from the acceptance check. Nothing here grades quality, and partial credit for a
        // task that failed in an interesting way would be a number nobody measured.
        let reward = if passed { 1.0 } else { 0.0 };
        match self.router.lock().await.reward(decision_id, reward, cost).await {
            Ok(outcome) => tracing::debug!(?outcome, decision_id, reward, "routing reward"),
            Err(e) => tracing::warn!(error = %e, "could not record a routing reward"),
        }
    }

    pub fn cost_of(&self, arm: &str) -> f64 {
        self.arms
            .iter()
            .find(|a| a.arm == arm)
            .map(|a| a.cost)
            .unwrap_or(1.0)
    }
}

/// The context a routing decision is made in.
///
/// Everything here is known before the task runs. A feature that needed the result would be a
/// feature the decision cannot use, and computing one from the result afterwards would train the
/// model on information it will not have next time.
pub fn features_for(task: &DraftTask, attempt: u32) -> Features {
    let body_len = task.body.len().max(1) as f64;
    let files = task.declared_paths.len().max(1) as f64;

    Features::new()
        .with(Features::PROMPT_SIZE, (body_len.log10() / 5.0).clamp(0.0, 1.0))
        .with(Features::CONTEXT_BREADTH, (files.log10() / 3.0).clamp(0.0, 1.0))
        // Every orchestrated task edits code: that is what a task is here. Left as a feature rather
        // than dropped because the same bandit will be asked about review and planning roles later,
        // and a feature that appears then would have no history behind it.
        .with(Features::IS_EDIT, 1.0)
        // The one feature that is always true in this system and is worth stating anyway: a task
        // without a decidable acceptance check never reaches dispatch, because validation rejects
        // the graph that contains it.
        .with(Features::HAS_VERIFIER, 1.0)
        .with(Features::ATTEMPT, (attempt as f64 / 3.0).clamp(0.0, 1.0))
        // Unattended. An orchestrated task has nobody watching it, which is the case where a slower
        // cheaper arm costs nothing but time.
        .with(Features::INTERACTIVE, 0.0)
        .clamped()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wkbd_orch::VerifySpec;

    fn task(body: &str, paths: &[&str]) -> DraftTask {
        DraftTask {
            id: "t".into(),
            title: "t".into(),
            body: body.into(),
            declared_paths: paths.iter().map(|s| s.to_string()).collect(),
            depends_on: vec![],
            verify: VerifySpec {
                setup: vec![],
                cmd: "cargo test".into(),
                must_pass: vec!["a::b".into()],
                must_still_pass: vec![],
                immutable_paths: vec![],
            },
        }
    }

    /// The exploration coefficient is calibrated against `sqrt(xᵀA⁻¹x)`, so a feature that ranges
    /// over thousands silently multiplies the exploration rate. Everything is scaled, and the clamp
    /// is the backstop rather than the mechanism.
    #[test]
    fn every_feature_stays_in_range() {
        let huge = "x".repeat(500_000);
        let f = features_for(&task(&huge, &vec!["p"; 5_000]), 99);
        for (i, v) in f.as_slice().iter().enumerate() {
            assert!((0.0..=1.0).contains(v), "feature {i} is {v}");
        }
    }

    #[test]
    fn a_bigger_task_reads_as_bigger() {
        let small = features_for(&task("do a thing", &["a.rs"]), 0);
        let large = features_for(&task(&"x".repeat(10_000), &["a.rs"]), 0);
        assert!(large.as_slice()[Features::PROMPT_SIZE] > small.as_slice()[Features::PROMPT_SIZE]);
    }

    /// A retry is evidence the cheap arm did not manage it, and the feature is what lets the model
    /// learn that rather than making the same choice again.
    #[test]
    fn a_retry_is_visible_to_the_model() {
        let first = features_for(&task("b", &["a.rs"]), 0);
        let third = features_for(&task("b", &["a.rs"]), 2);
        assert!(third.as_slice()[Features::ATTEMPT] > first.as_slice()[Features::ATTEMPT]);
    }

    /// A bandit over one arm is arithmetic with a fixed answer, and recording decisions for it fills
    /// the table with rows nothing can ever learn from.
    #[tokio::test]
    async fn one_agent_is_not_routed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap().store;
        let routing = Routing::new(store, &["only".to_string()], &BTreeMap::new())
            .await
            .unwrap();
        assert!(!routing.is_routing());
        assert!(routing.choose(&task("b", &["a.rs"]), 0).await.is_none());
    }

    #[tokio::test]
    async fn two_agents_produce_a_decision_naming_one_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap().store;
        let ids = vec!["cheap".to_string(), "dear".to_string()];
        let routing = Routing::new(store, &ids, &BTreeMap::new()).await.unwrap();
        assert!(routing.is_routing());

        let decision = routing.choose(&task("b", &["a.rs"]), 0).await.expect("a decision");
        assert!(ids.contains(&decision.arm), "{}", decision.arm);
        assert_eq!(decision.role, WORKER);
    }

    /// Declared, not measured. Absent means 1.0, which makes the price half of the objective inert —
    /// stated here so that a later reader does not mistake a working router for a working cost
    /// control.
    #[tokio::test]
    async fn costs_come_from_what_somebody_declared() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap().store;
        let mut costs = BTreeMap::new();
        costs.insert("dear".to_string(), 8.0);
        let routing = Routing::new(
            store,
            &["cheap".to_string(), "dear".to_string()],
            &costs,
        )
        .await
        .unwrap();
        assert_eq!(routing.cost_of("dear"), 8.0);
        assert_eq!(routing.cost_of("cheap"), 1.0);
        assert_eq!(routing.cost_of("never-heard-of-it"), 1.0);
    }
}
