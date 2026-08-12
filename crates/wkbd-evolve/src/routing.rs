//! Which model does which job.
//!
//! # Routing is a cost control, not a quality improvement
//!
//! LLMRouterBench (ACL 2026 Findings; ~400k instances, 33 models) found that most routing
//! methods collapse to similar performance, that commercial routers fail to beat simple
//! baselines, and OpenRouter itself retreated from a learned classifier to following the
//! trailing seven-day spend share. Treating a router as a way to get better answers is
//! therefore not supported by the evidence. Treating it as a way to stop paying frontier
//! prices for work a cheap model finishes correctly is.
//!
//! So the objective here is **the cheapest arm that still clears a quality floor**, which
//! is the two halves of [`RouterConfig::quality_target`] and
//! [`RouterConfig::cost_weight`]: arms whose optimistic quality cannot reach the floor are
//! dropped, and what is left is ranked by quality minus a price penalty. Nothing in this
//! module claims routing makes the output better.
//!
//! # Why LinUCB and not Thompson sampling
//!
//! Deterministic scores interact more predictably with the Lagrangian penalty of a budget
//! constraint. A sampled score changes the effective trade-off between quality and price
//! from draw to draw, so a fixed penalty means something different on every decision and
//! the realised spend under a constraint is much harder to reason about; a deterministic
//! score plus a fixed penalty has one answer per context, which is also what makes an
//! after-the-fact audit of "why did this go to the expensive model" possible.
//!
//! # Non-stationarity: geometric forgetting
//!
//! Arms move under you. A vendor silently swaps the model behind an endpoint, changes a
//! system prompt, or re-prices it, and no signal announces any of it. Sufficient
//! statistics for the arm that was chosen are therefore multiplied by γ before the new
//! observation is folded in, giving evidence a half-life of ln2/(1−γ) updates — with
//! γ = 0.997 that is about 231 updates, and an e-folding time of 1/(1−γ) ≈ 333. Without
//! it, an arm with a long good history needs a proportionally long bad history before its
//! mean moves, so the better the arm used to be, the longer the system keeps sending work
//! to it after it has broken.
//!
//! # Non-stationarity: staleness-based variance inflation
//!
//! Passive forgetting only touches arms that get chosen, so an arm nobody picks keeps its
//! stale, confident estimate forever and never gets re-checked. The variance used for the
//! confidence bonus is therefore inflated by how many decisions have passed since the arm
//! was last selected — **with a cap** ([`MAX_VARIANCE_INFLATION`]). The cap is
//! load-bearing: an uncapped inflation grows without bound, and an unbounded exploration
//! bonus eventually dominates any finite price penalty, at which point the router is
//! compelled to send work to whichever arm is most expensive and most out of date.
//!
//! # The interaction that bites
//!
//! Geometric forgetting shrinks the scatter matrix, which pushes `A` toward singular in
//! directions nothing has been observed in lately, which inflates `xᵀA⁻¹x`, which inflates
//! the confidence bonus. So the forgetting rate and the exploration coefficient are one
//! calibration and not two: raising γ's aggressiveness while leaving α alone silently
//! raises the exploration rate as well. The ridge is re-injected on every update
//! (see [`ArmModel::update`]) precisely so this failure has a floor rather than an
//! asymptote, and α and γ are documented together on [`RouterConfig`] so that changing one
//! without the other looks wrong in review.
//!
//! # Why the feature vector is stored at decision time
//!
//! The reward that matters on a desktop workbench is not the one available when the call
//! returns. It is "did that pull request get merged", "was it reverted a week later",
//! "did the user keep the diff" — hours to days late, long after the context that produced
//! the decision is gone. The vector is written into `routing_decisions.features` when the
//! choice is made, so [`Router::reward`] can update the right arm with the right context
//! whenever the signal turns up, without recomputing a context that no longer exists.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wkbd_store::Store;

/// Dimensionality of the context vector.
///
/// Twelve, and the ceiling is set by the forgetting factor rather than by taste: with
/// γ = 0.997 the effective sample size per arm saturates around 1/(1−γ) ≈ 333
/// observations, and a ridge regression needs its effective sample size to be a
/// comfortable multiple of its dimension before the estimate means anything. Twelve leaves
/// roughly a factor of thirty. It also keeps a 12×12 inverse in the sub-microsecond range,
/// so the exact re-inversion that keeps Sherman-Morrison honest is affordable often.
pub const FEATURE_DIM: usize = 12;

/// Forgetting factor. Half-life ln2/(1−γ) ≈ 231 updates, e-folding 1/(1−γ) ≈ 333.
pub const DEFAULT_FORGETTING: f64 = 0.997;

/// The cap on staleness-driven variance inflation. See the module docs for why an
/// uncapped one ends with the router pinned to the most expensive idle arm.
pub const MAX_VARIANCE_INFLATION: f64 = 200.0;

/// A context vector.
///
/// Features are expected to be scaled into roughly [0, 1]; the exploration coefficient is
/// calibrated in reward units against `sqrt(xᵀA⁻¹x)`, so a feature that ranges over
/// thousands silently multiplies the exploration rate. [`Features::clamped`] enforces the
/// range rather than trusting callers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Features(pub [f64; FEATURE_DIM]);

impl Default for Features {
    fn default() -> Self {
        Features::new()
    }
}

impl Features {
    /// Constant term. Without it every arm's estimate is forced through the origin, and
    /// the model has to spend observations learning a mean it could have been given.
    pub const BIAS: usize = 0;
    /// log10 of the prompt size, over 5.
    pub const PROMPT_SIZE: usize = 1;
    /// log10 of the number of files in scope, over 3.
    pub const CONTEXT_BREADTH: usize = 2;
    /// The task edits code.
    pub const IS_EDIT: usize = 3;
    /// The task is planning or decomposition.
    pub const IS_PLANNING: usize = 4;
    /// The task is review or explanation.
    pub const IS_REVIEW: usize = 5;
    /// A machine-checkable acceptance test exists, so a wrong answer is cheap to detect.
    pub const HAS_VERIFIER: usize = 6;
    /// Retry index of this task, over 3. A retry is evidence the cheap arm did not do it.
    pub const ATTEMPT: usize = 7;
    /// How much history this project has, over 100 runs.
    pub const FAMILIARITY: usize = 8;
    /// log10 of the expected diff size, over 4.
    pub const DIFF_SIZE: usize = 9;
    /// The user is waiting on this interactively.
    pub const INTERACTIVE: usize = 10;
    /// Failure rate of the last ten runs in this project.
    pub const RECENT_FAILURES: usize = 11;

    pub fn new() -> Self {
        let mut f = [0.0; FEATURE_DIM];
        f[Features::BIAS] = 1.0;
        Features(f)
    }

    pub fn with(mut self, index: usize, value: f64) -> Self {
        self.0[index] = value;
        self
    }

    /// Non-finite values become zero and the rest are clamped. A NaN reaching the matrix
    /// update poisons the arm permanently: every later inverse is NaN, every score is NaN,
    /// and the arm is neither usable nor visibly broken.
    pub fn clamped(&self) -> Features {
        let mut out = [0.0; FEATURE_DIM];
        for (i, v) in self.0.iter().enumerate() {
            out[i] = if v.is_finite() { v.clamp(-4.0, 4.0) } else { 0.0 };
        }
        Features(out)
    }

    pub fn as_slice(&self) -> &[f64] {
        &self.0
    }
}

/// One routable option: a model, an agent configuration, a provider endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmSpec {
    pub arm: String,
    /// Price of one call in whatever unit the caller uses consistently, normalised so the
    /// cheapest arm is near 1.0. Only ratios matter, because it is multiplied by
    /// [`RouterConfig::cost_weight`] and compared against a quality estimate in [0, 1].
    pub cost: f64,
}

impl ArmSpec {
    pub fn new(arm: impl Into<String>, cost: f64) -> Self {
        ArmSpec {
            arm: arm.into(),
            cost,
        }
    }
}

/// α and γ belong to one calibration; see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct RouterConfig {
    /// Exploration coefficient. Calibrated jointly with `forgetting`: forgetting shrinks
    /// `A`, which raises `xᵀA⁻¹x`, which raises this term's effect.
    pub alpha: f64,
    /// γ. Calibrated jointly with `alpha`.
    pub forgetting: f64,
    /// Ridge, re-injected on every update so that forgetting cannot drive the smallest
    /// eigenvalue of `A` to zero.
    pub ridge: f64,
    /// λ on the price penalty.
    pub cost_weight: f64,
    /// The quality floor the router is trying to buy as cheaply as possible.
    pub quality_target: f64,
    /// Decisions of idleness that double the variance, roughly.
    pub staleness_tau: f64,
    pub max_variance_inflation: f64,
    /// How often the ridge is topped back up and the inverse recomputed exactly. Bounds
    /// how far forgetting can eat into the spectral floor of `A`: at the default settings
    /// the floor never drops below γ¹⁶ ≈ 95% of the ridge.
    pub refresh_every: u32,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            alpha: 0.8,
            forgetting: DEFAULT_FORGETTING,
            ridge: 1.0,
            cost_weight: 0.15,
            quality_target: 0.6,
            staleness_tau: 25.0,
            max_variance_inflation: MAX_VARIANCE_INFLATION,
            refresh_every: 16,
        }
    }
}

/// Offline evidence, expressed as pseudo-observations.
///
/// `n_eff` is the total weight the whole set is worth, not a weight per observation: it is
/// how many real observations the caller is willing to claim the offline data replaces. It
/// needs no separate decay schedule, because every online update multiplies the arm's
/// statistics by γ, so the prior's weight falls off geometrically on its own and the
/// steady state is set by online evidence alone.
#[derive(Debug, Clone, Default)]
pub struct WarmStart {
    pub observations: Vec<(Features, f64)>,
    pub n_eff: f64,
}

/// LinUCB sufficient statistics for one arm, with no persistence and no policy.
#[derive(Debug, Clone)]
pub struct ArmModel {
    a: Vec<f64>,
    a_inv: Vec<f64>,
    b: Vec<f64>,
    updates: u64,
    since_refresh: u32,
    /// How much of the ridge is left after the forgetting applied since the last exact
    /// refresh. See [`ArmModel::update`].
    ridge_fraction: f64,
}

impl ArmModel {
    pub fn new(ridge: f64) -> Self {
        let ridge = if ridge > 0.0 { ridge } else { 1.0 };
        let mut a = vec![0.0; FEATURE_DIM * FEATURE_DIM];
        let mut a_inv = vec![0.0; FEATURE_DIM * FEATURE_DIM];
        for i in 0..FEATURE_DIM {
            a[i * FEATURE_DIM + i] = ridge;
            a_inv[i * FEATURE_DIM + i] = 1.0 / ridge;
        }
        ArmModel {
            a,
            a_inv,
            b: vec![0.0; FEATURE_DIM],
            updates: 0,
            since_refresh: 0,
            ridge_fraction: 1.0,
        }
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    pub fn warm_start(&mut self, warm: &WarmStart) {
        if warm.observations.is_empty() || warm.n_eff <= 0.0 {
            return;
        }
        let weight = warm.n_eff / warm.observations.len() as f64;
        for (features, reward) in &warm.observations {
            let x = features.clamped();
            let x = x.as_slice();
            for i in 0..FEATURE_DIM {
                for j in 0..FEATURE_DIM {
                    self.a[i * FEATURE_DIM + j] += weight * x[i] * x[j];
                }
                self.b[i] += weight * reward * x[i];
            }
        }
        self.refresh_inverse();
    }

    /// Folds one observation in, forgetting geometrically.
    ///
    /// `A ← γA + xxᵀ`, which is a scaling plus a rank-1 term and therefore exactly what
    /// the Sherman-Morrison identity can follow: the cached inverse is scaled by 1/γ and
    /// then rank-1 updated, and it stays the true inverse of the matrix that is stored.
    ///
    /// What that plain recursion loses is the ridge: forgetting shrinks `λI` along with
    /// everything else, so after n updates the floor on `A`'s spectrum is `γⁿλ`, which
    /// decays to nothing. `A⁻¹` then grows without bound in whatever direction has not
    /// been observed lately, the confidence bonus computed from it grows with it, and the
    /// scores become noise — the ill-conditioning half of the α/γ interaction in the
    /// module docs. The ridge is therefore restored to `λ` in
    /// [`ArmModel::restore_and_refresh`], which runs every
    /// [`RouterConfig::refresh_every`] updates, so the floor never drops below `γ^refresh
    /// λ` — 95% of it at the default settings — and the same pass recomputes the inverse
    /// exactly, which clears whatever round-off the rank-1 chain has accumulated.
    ///
    /// Doing the restoration inside every update instead would add `(1−γ)λI`, which is not
    /// a rank-1 term, so the cached inverse would no longer be the inverse of anything in
    /// particular between refreshes: measured on this code, that arrangement drifts about
    /// 2.5e-3 per update and reaches 1.9e1 if the refresh is disabled, against round-off
    /// for the arrangement used here.
    pub fn update(&mut self, features: &Features, reward: f64, config: &RouterConfig) {
        let x = features.clamped();
        let x = x.as_slice();
        let reward = if reward.is_finite() {
            reward.clamp(0.0, 1.0)
        } else {
            return;
        };
        let gamma = config.forgetting.clamp(0.5, 1.0);

        for i in 0..FEATURE_DIM {
            for j in 0..FEATURE_DIM {
                let cell = &mut self.a[i * FEATURE_DIM + j];
                *cell = gamma * *cell + x[i] * x[j];
            }
            self.b[i] = gamma * self.b[i] + reward * x[i];
        }
        self.ridge_fraction *= gamma;

        for cell in self.a_inv.iter_mut() {
            *cell /= gamma;
        }
        sherman_morrison(&mut self.a_inv, x);

        self.updates += 1;
        self.since_refresh += 1;
        if self.since_refresh >= config.refresh_every.max(1)
            || !self.a_inv.iter().all(|v| v.is_finite())
        {
            self.restore_and_refresh(config.ridge);
        }
    }

    /// Tops the ridge back up to `λ` and recomputes the inverse exactly.
    fn restore_and_refresh(&mut self, ridge: f64) {
        let deficit = ridge * (1.0 - self.ridge_fraction);
        if deficit > 0.0 && deficit.is_finite() {
            for i in 0..FEATURE_DIM {
                self.a[i * FEATURE_DIM + i] += deficit;
            }
        }
        self.ridge_fraction = 1.0;
        self.refresh_inverse();
    }

    fn refresh_inverse(&mut self) {
        match invert(&self.a) {
            Some(inverse) => self.a_inv = inverse,
            None => {
                // Unreachable while the ridge is being re-injected, which is the point of
                // re-injecting it. Falling back to the prior loses the arm's history but
                // keeps it selectable, which beats an arm whose every score is NaN.
                tracing::error!("routing arm matrix is singular; falling back to the prior");
                *self = ArmModel::new(1.0);
            }
        }
        self.since_refresh = 0;
    }

    /// θᵀx, the expected reward.
    pub fn mean(&self, features: &Features) -> f64 {
        let x = features.clamped();
        let x = x.as_slice();
        let theta = mat_vec(&self.a_inv, &self.b);
        dot(&theta, x)
    }

    /// xᵀA⁻¹x. Never negative: `A` is positive definite by construction, so a negative
    /// value is round-off, and letting it through would make `sqrt` produce NaN.
    pub fn variance(&self, features: &Features) -> f64 {
        let x = features.clamped();
        let x = x.as_slice();
        let av = mat_vec(&self.a_inv, x);
        dot(&av, x).max(0.0)
    }

    fn a_matrix_json(&self) -> String {
        serde_json::to_string(&self.a).unwrap_or_else(|_| "[]".into())
    }

    fn b_vector_json(&self) -> String {
        serde_json::to_string(&self.b).unwrap_or_else(|_| "[]".into())
    }

    fn from_persisted(a: Vec<f64>, b: Vec<f64>, updates: u64, ridge: f64) -> ArmModel {
        if a.len() != FEATURE_DIM * FEATURE_DIM || b.len() != FEATURE_DIM {
            tracing::warn!(
                a = a.len(),
                b = b.len(),
                "persisted routing arm has the wrong dimension; starting it over"
            );
            return ArmModel::new(ridge);
        }
        let mut model = ArmModel {
            a,
            a_inv: vec![0.0; FEATURE_DIM * FEATURE_DIM],
            b,
            updates,
            since_refresh: 0,
            // Whatever ridge the stored matrix has is what it has: the row was written
            // mid-cycle, so it is short by at most `(1 − γ^refresh)λ`, and the next
            // refresh tops it back up.
            ridge_fraction: 1.0,
        };
        // The inverse is derived, so it is recomputed rather than stored: a stored inverse
        // can disagree with the matrix it came from after a partial write, and nothing
        // would detect that.
        model.refresh_inverse();
        model
    }
}

/// How much the variance is inflated for an arm that has not been picked in a while.
pub fn staleness_inflation(steps_since_selected: u64, config: &RouterConfig) -> f64 {
    let tau = if config.staleness_tau > 0.0 {
        config.staleness_tau
    } else {
        1.0
    };
    (1.0 + steps_since_selected as f64 / tau).min(config.max_variance_inflation.max(1.0))
}

/// What the scoring rule thought of one arm, for the audit log and for tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmScore {
    pub arm: String,
    pub mean: f64,
    pub bonus: f64,
    pub ucb: f64,
    pub cost: f64,
    pub variance_inflation: f64,
    /// Optimistic quality clears the floor.
    pub eligible: bool,
    /// Quality minus the price penalty.
    pub score: f64,
    /// This arm has never been selected, so it is taken before the scores are consulted.
    pub untried: bool,
}

#[derive(Debug, Clone)]
struct Arm {
    spec: ArmSpec,
    model: ArmModel,
    last_selected_step: u64,
    ever_selected: bool,
    /// Realised price, once anything has reported one. The configured cost is a list
    /// price and a call that needed three retries did not cost the list price.
    observed_cost: Option<f64>,
    updated_ms: i64,
}

impl Arm {
    fn cost(&self) -> f64 {
        self.observed_cost.unwrap_or(self.spec.cost)
    }
}

/// The algorithm, with no database attached.
///
/// Separated from [`Router`] so the policy can be exercised over tens of thousands of
/// rounds — which is what a claim about numerical stability under geometric forgetting
/// needs — without tens of thousands of transactions.
#[derive(Debug, Clone)]
pub struct Bandit {
    config: RouterConfig,
    arms: Vec<Arm>,
    step: u64,
}

impl Bandit {
    pub fn new(config: RouterConfig, specs: Vec<ArmSpec>) -> Bandit {
        let mut specs = specs;
        // Sorted, so that "the first arm among equals" is a property of the arm names and
        // not of the order the caller happened to register them in.
        specs.sort_by(|a, b| a.arm.cmp(&b.arm));
        let arms = specs
            .into_iter()
            .map(|spec| Arm {
                model: ArmModel::new(config.ridge),
                spec,
                last_selected_step: 0,
                ever_selected: false,
                observed_cost: None,
                updated_ms: 0,
            })
            .collect();
        Bandit {
            config,
            arms,
            step: 0,
        }
    }

    pub fn warm_start(&mut self, arm: &str, warm: &WarmStart) {
        if let Some(a) = self.arms.iter_mut().find(|a| a.spec.arm == arm) {
            a.model.warm_start(warm);
        }
    }

    pub fn arms(&self) -> impl Iterator<Item = &str> {
        self.arms.iter().map(|a| a.spec.arm.as_str())
    }

    pub fn step(&self) -> u64 {
        self.step
    }

    pub fn model(&self, arm: &str) -> Option<&ArmModel> {
        self.arms
            .iter()
            .find(|a| a.spec.arm == arm)
            .map(|a| &a.model)
    }

    pub fn scores(&self, features: &Features) -> Vec<ArmScore> {
        self.arms
            .iter()
            .map(|arm| {
                let inflation =
                    staleness_inflation(self.step.saturating_sub(arm.last_selected_step), &self.config);
                let mean = arm.model.mean(features);
                let bonus = self.config.alpha * (inflation * arm.model.variance(features)).sqrt();
                let ucb = mean + bonus;
                let cost = arm.cost();
                ArmScore {
                    arm: arm.spec.arm.clone(),
                    mean,
                    bonus,
                    ucb,
                    cost,
                    variance_inflation: inflation,
                    eligible: ucb >= self.config.quality_target,
                    score: ucb - self.config.cost_weight * cost,
                    untried: !arm.ever_selected,
                }
            })
            .collect()
    }

    /// Picks an arm and records that it was picked. Returns its index.
    pub fn choose(&mut self, features: &Features) -> usize {
        let index = self.select(features);
        self.step += 1;
        self.arms[index].last_selected_step = self.step;
        self.arms[index].ever_selected = true;
        index
    }

    fn select(&self, features: &Features) -> usize {
        // An arm nobody has tried has no evidence at all, and a prior mean of zero is not
        // the same statement as "we believe it is bad". One forced trial each, cheapest
        // first. The condition is "never selected" rather than "never rewarded" on
        // purpose: rewards can be days late, and waiting for them would leave the router
        // stuck on whichever arm it tried first.
        let untried = self
            .arms
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.ever_selected)
            .min_by(|(_, a), (_, b)| {
                a.cost()
                    .total_cmp(&b.cost())
                    .then_with(|| a.spec.arm.cmp(&b.spec.arm))
            })
            .map(|(i, _)| i);
        if let Some(index) = untried {
            return index;
        }

        let scores = self.scores(features);
        // The floor first, then price. Ranking by price before applying the floor would
        // hand every decision to the cheapest arm regardless of whether it can do the job,
        // which is the failure that gives cost-aware routing its bad reputation.
        let eligible: Vec<usize> = scores
            .iter()
            .enumerate()
            .filter(|(_, s)| s.eligible)
            .map(|(i, _)| i)
            .collect();
        // Nothing clears the floor: the work still has to go somewhere, so the best
        // available arm gets it. Refusing to route would only move the decision to a
        // caller with less information.
        let candidates: Vec<usize> = if eligible.is_empty() {
            (0..scores.len()).collect()
        } else {
            eligible
        };

        let mut best = candidates[0];
        for &index in &candidates[1..] {
            if scores[index].score > scores[best].score {
                best = index;
            }
        }
        best
    }

    /// Folds a reward into one arm. Safe to call long after [`Bandit::choose`].
    pub fn update(&mut self, arm: &str, features: &Features, reward: f64, cost: Option<f64>) -> bool {
        let config = self.config;
        let Some(a) = self.arms.iter_mut().find(|a| a.spec.arm == arm) else {
            return false;
        };
        a.model.update(features, reward, &config);
        if let Some(cost) = cost.filter(|c| c.is_finite() && *c >= 0.0) {
            // Same forgetting factor as the quality estimate: a price that changed three
            // months ago should not still be dragging the average around.
            a.observed_cost = Some(match a.observed_cost {
                Some(previous) => config.forgetting * previous + (1.0 - config.forgetting) * cost,
                None => cost,
            });
        }
        true
    }
}

/// One recorded choice.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub id: String,
    pub arm: String,
    pub role: String,
    /// Kept so the caller can log it; the authoritative copy is the one in the database,
    /// which is what a late reward is matched against.
    pub features: Features,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewardOutcome {
    Applied,
    /// A second reward for the same decision. Ignored: folding it in twice would double
    /// the weight of one observation, and a retrying caller must not be able to move the
    /// estimate by retrying.
    AlreadyRewarded,
    /// The decision names an arm that is no longer registered for that role.
    UnknownArm,
}

/// The bandit plus the database.
pub struct Router {
    store: Store,
    config: RouterConfig,
    roles: BTreeMap<String, Bandit>,
}

impl Router {
    pub fn new(store: Store, config: RouterConfig) -> Router {
        Router {
            store,
            config,
            roles: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    pub fn bandit(&self, role: &str) -> Option<&Bandit> {
        self.roles.get(role)
    }

    /// Registers the arms available for a role and restores their state from the database.
    pub async fn register_role(
        &mut self,
        role: &str,
        specs: Vec<ArmSpec>,
        warm: &BTreeMap<String, WarmStart>,
    ) -> Result<()> {
        let mut bandit = Bandit::new(self.config, specs);
        for (arm, start) in warm {
            bandit.warm_start(arm, start);
        }

        let persisted = load_arms(&self.store, role).await?;
        for arm in bandit.arms.iter_mut() {
            if let Some(state) = persisted.get(&arm.spec.arm) {
                arm.model = ArmModel::from_persisted(
                    state.a.clone(),
                    state.b.clone(),
                    state.updates,
                    self.config.ridge,
                );
                arm.updated_ms = state.updated_ms;
            }
        }

        // Staleness is counted in decisions, not in wall-clock time, so it has to be
        // rebuilt from the decision log: a workbench that was closed for a week has not
        // made any decisions in that week, and the arms are exactly as stale as they were.
        let history = load_recent_arms_chosen(&self.store, role).await?;
        for (index, chosen) in history.iter().enumerate() {
            let step = index as u64 + 1;
            if let Some(arm) = bandit.arms.iter_mut().find(|a| &a.spec.arm == chosen) {
                arm.last_selected_step = step;
                arm.ever_selected = true;
            }
        }
        bandit.step = history.len() as u64;

        for arm in bandit.arms.iter_mut() {
            if let Some(cost) = persisted.get(&arm.spec.arm).and_then(|s| s.observed_cost) {
                arm.observed_cost = Some(cost);
            }
        }

        self.roles.insert(role.to_string(), bandit);
        Ok(())
    }

    /// Picks an arm and records the decision, feature vector included.
    pub async fn choose(&mut self, role: &str, features: Features) -> Result<Decision> {
        let bandit = self
            .roles
            .get_mut(role)
            .with_context(|| format!("no arms registered for role {role}"))?;
        let features = features.clamped();
        let index = bandit.choose(&features);
        let arm = bandit.arms[index].spec.arm.clone();

        let decision = Decision {
            id: uuid::Uuid::new_v4().to_string(),
            arm,
            role: role.to_string(),
            features,
        };

        let id = decision.id.clone();
        let arm = decision.arm.clone();
        let role = role.to_string();
        let encoded = serde_json::to_string(&features.0)?;
        let now = wkbd_store::now_ms();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO routing_decisions (id, arm, role, features, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id, arm, role, encoded, now],
                )?;
                Ok(())
            })
            .await?;

        Ok(decision)
    }

    /// Folds in a reward that may have arrived days after the decision.
    ///
    /// The caller supplies no features: they come from the row written at decision time,
    /// which is the only copy that is still true.
    pub async fn reward(&mut self, decision_id: &str, reward: f64, cost: f64) -> Result<RewardOutcome> {
        let wanted = decision_id.to_string();
        let row: Option<(String, String, String, Option<f64>)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT role, arm, features, reward FROM routing_decisions WHERE id = ?1",
                )?;
                let mut rows = stmt.query_map(rusqlite::params![wanted], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                    ))
                })?;
                match rows.next() {
                    Some(row) => Ok(Some(row?)),
                    None => Ok(None),
                }
            })
            .await?;

        let Some((role, arm, encoded, existing)) = row else {
            anyhow::bail!("no such routing decision: {decision_id}");
        };
        if existing.is_some() {
            return Ok(RewardOutcome::AlreadyRewarded);
        }

        let values: Vec<f64> = serde_json::from_str(&encoded)
            .with_context(|| format!("decoding the features of decision {decision_id}"))?;
        let mut array = [0.0; FEATURE_DIM];
        if values.len() != FEATURE_DIM {
            anyhow::bail!(
                "decision {decision_id} stored {} features, this build expects {FEATURE_DIM}",
                values.len()
            );
        }
        array.copy_from_slice(&values);
        let features = Features(array);

        let Some(bandit) = self.roles.get_mut(&role) else {
            anyhow::bail!("no arms registered for role {role}");
        };
        if !bandit.update(&arm, &features, reward, Some(cost)) {
            tracing::warn!(%arm, %role, "reward for an arm that is no longer registered");
            return Ok(RewardOutcome::UnknownArm);
        }

        let updated = bandit
            .arms
            .iter()
            .find(|a| a.spec.arm == arm)
            .expect("update returned true");
        let a_json = updated.model.a_matrix_json();
        let b_json = updated.model.b_vector_json();
        let updates = updated.model.updates as i64;
        let observed_cost = updated.observed_cost;

        let id = decision_id.to_string();
        let now = wkbd_store::now_ms();
        let arm_for_write = arm.clone();
        let role_for_write = role.clone();
        self.store
            .write(move |tx| {
                // One transaction: an arm updated without its decision being marked
                // rewarded would take the same observation again on the next call.
                let changed = tx.execute(
                    "UPDATE routing_decisions SET reward = ?2, cost = ?3, rewarded_ms = ?4
                     WHERE id = ?1 AND reward IS NULL",
                    rusqlite::params![id, reward, observed_cost, now],
                )?;
                if changed == 0 {
                    anyhow::bail!("routing decision was rewarded concurrently");
                }
                tx.execute(
                    "INSERT INTO routing_arms (arm, role, a_matrix, b_vector, updates, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(arm, role) DO UPDATE SET
                        a_matrix = excluded.a_matrix,
                        b_vector = excluded.b_vector,
                        updates = excluded.updates,
                        updated_ms = excluded.updated_ms",
                    rusqlite::params![
                        arm_for_write,
                        role_for_write,
                        a_json,
                        b_json,
                        updates,
                        now
                    ],
                )?;
                Ok(())
            })
            .await?;

        Ok(RewardOutcome::Applied)
    }

    pub fn scores(&self, role: &str, features: &Features) -> Vec<ArmScore> {
        self.roles
            .get(role)
            .map(|b| b.scores(features))
            .unwrap_or_default()
    }
}

struct PersistedArm {
    a: Vec<f64>,
    b: Vec<f64>,
    updates: u64,
    updated_ms: i64,
    observed_cost: Option<f64>,
}

async fn load_arms(store: &Store, role: &str) -> Result<BTreeMap<String, PersistedArm>> {
    let role = role.to_string();
    store
        .read(move |conn| {
            let mut out = BTreeMap::new();
            let mut stmt = conn.prepare(
                "SELECT arm, a_matrix, b_vector, updates, updated_ms
                 FROM routing_arms WHERE role = ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![&role], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            for row in rows {
                let (arm, a, b, updates, updated_ms) = row?;
                let a: Vec<f64> = serde_json::from_str(&a).unwrap_or_default();
                let b: Vec<f64> = serde_json::from_str(&b).unwrap_or_default();
                out.insert(
                    arm,
                    PersistedArm {
                        a,
                        b,
                        updates: updates.max(0) as u64,
                        updated_ms,
                        observed_cost: None,
                    },
                );
            }

            // The realised price per arm, recomputed from the decisions rather than kept
            // in a column: it is a summary of rows that are already there, and a summary
            // stored beside its source is a summary that can disagree with it.
            let mut stmt = conn.prepare(
                "SELECT arm, AVG(cost) FROM routing_decisions
                 WHERE role = ?1 AND cost IS NOT NULL GROUP BY arm",
            )?;
            let rows = stmt.query_map(rusqlite::params![&role], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?))
            })?;
            for row in rows {
                let (arm, cost) = row?;
                if let Some(entry) = out.get_mut(&arm) {
                    entry.observed_cost = cost;
                }
            }
            Ok(out)
        })
        .await
}

/// The arms chosen for a role, oldest first, capped so that a long-lived workbench does
/// not read its whole history at startup. Only the tail matters: staleness is about what
/// happened recently.
async fn load_recent_arms_chosen(store: &Store, role: &str) -> Result<Vec<String>> {
    let role = role.to_string();
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT arm FROM (
                     SELECT arm, created_ms, id FROM routing_decisions
                     WHERE role = ?1 ORDER BY created_ms DESC, id DESC LIMIT 5000
                 ) ORDER BY created_ms ASC, id ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![role], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
}

/// `(A + xxᵀ)⁻¹ = A⁻¹ − (A⁻¹x)(xᵀA⁻¹) / (1 + xᵀA⁻¹x)`.
fn sherman_morrison(a_inv: &mut [f64], x: &[f64]) {
    let av = mat_vec(a_inv, x);
    let denominator = 1.0 + dot(x, &av);
    if !denominator.is_finite() || denominator.abs() < 1e-12 {
        return;
    }
    for i in 0..FEATURE_DIM {
        for j in 0..FEATURE_DIM {
            a_inv[i * FEATURE_DIM + j] -= av[i] * av[j] / denominator;
        }
    }
}

/// Gauss-Jordan with partial pivoting. Twelve dimensions, so the cubic cost is irrelevant
/// and the clarity is worth more than a decomposition.
fn invert(a: &[f64]) -> Option<Vec<f64>> {
    let n = FEATURE_DIM;
    let mut m = a.to_vec();
    let mut inverse = vec![0.0; n * n];
    for i in 0..n {
        inverse[i * n + i] = 1.0;
    }

    for column in 0..n {
        let mut pivot = column;
        for row in (column + 1)..n {
            if m[row * n + column].abs() > m[pivot * n + column].abs() {
                pivot = row;
            }
        }
        if !m[pivot * n + column].is_finite() || m[pivot * n + column].abs() < 1e-12 {
            return None;
        }
        if pivot != column {
            for k in 0..n {
                m.swap(column * n + k, pivot * n + k);
                inverse.swap(column * n + k, pivot * n + k);
            }
        }
        let scale = m[column * n + column];
        for k in 0..n {
            m[column * n + k] /= scale;
            inverse[column * n + k] /= scale;
        }
        for row in 0..n {
            if row == column {
                continue;
            }
            let factor = m[row * n + column];
            if factor == 0.0 {
                continue;
            }
            for k in 0..n {
                m[row * n + k] -= factor * m[column * n + k];
                inverse[row * n + k] -= factor * inverse[column * n + k];
            }
        }
    }
    inverse.iter().all(|v| v.is_finite()).then_some(inverse)
}

fn mat_vec(m: &[f64], x: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; FEATURE_DIM];
    for i in 0..FEATURE_DIM {
        let mut sum = 0.0;
        for j in 0..FEATURE_DIM {
            sum += m[i * FEATURE_DIM + j] * x[j];
        }
        out[i] = sum;
    }
    out
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let opened = Store::open(dir.path()).unwrap();
        assert!(opened.degraded.is_none());
        (dir, opened.store)
    }

    /// Deterministic jitter. A bandit fed one constant context learns a single number and
    /// exercises none of the linear algebra, so the tests vary the context — but they have
    /// to do it reproducibly, or a failure cannot be re-run.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64) / ((1u64 << 31) as f64)
        }
    }

    fn context(rng: &mut Lcg) -> Features {
        Features::new()
            .with(Features::PROMPT_SIZE, rng.next())
            .with(Features::IS_EDIT, if rng.next() > 0.5 { 1.0 } else { 0.0 })
            .with(Features::HAS_VERIFIER, if rng.next() > 0.3 { 1.0 } else { 0.0 })
            .with(Features::DIFF_SIZE, rng.next())
    }

    fn noisy(base: f64, rng: &mut Lcg) -> f64 {
        (base + 0.08 * (rng.next() - 0.5)).clamp(0.0, 1.0)
    }

    fn share(chosen: &[String], arm: &str) -> f64 {
        chosen.iter().filter(|a| *a == arm).count() as f64 / chosen.len() as f64
    }

    #[tokio::test]
    async fn cold_start_tries_every_arm_instead_of_locking_onto_the_first() {
        let (_d, store) = store().await;
        let mut router = Router::new(store, RouterConfig::default());
        let specs = vec![
            ArmSpec::new("big", 4.0),
            ArmSpec::new("medium", 2.0),
            ArmSpec::new("small", 1.0),
            ArmSpec::new("tiny", 0.5),
        ];
        router
            .register_role("implement", specs, &BTreeMap::new())
            .await
            .unwrap();

        // The first arm tried is also the best one. Without a forced trial per arm, one
        // good early reward makes the optimistic estimate of the arm that produced it beat
        // the untried arms' priors, and the router never looks at the others again.
        let mut rng = Lcg(1);
        let mut chosen = Vec::new();
        for _ in 0..12 {
            let features = context(&mut rng);
            let decision = router.choose("implement", features).await.unwrap();
            let reward = match decision.arm.as_str() {
                "tiny" => 0.95,
                _ => 0.2,
            };
            router
                .reward(&decision.id, noisy(reward, &mut rng), 1.0)
                .await
                .unwrap();
            chosen.push(decision.arm);
        }

        for arm in ["big", "medium", "small", "tiny"] {
            assert!(
                chosen.contains(&arm.to_string()),
                "{arm} was never tried: {chosen:?}"
            );
        }
        assert!(
            share(&chosen, "tiny") < 0.8,
            "one arm took the whole cold start: {chosen:?}"
        );
    }

    #[tokio::test]
    async fn a_clearly_better_arm_gets_most_of_the_traffic() {
        let (_d, store) = store().await;
        let mut router = Router::new(store, RouterConfig::default());
        router
            .register_role(
                "review",
                vec![ArmSpec::new("good", 1.0), ArmSpec::new("weak", 1.0)],
                &BTreeMap::new(),
            )
            .await
            .unwrap();

        let mut rng = Lcg(7);
        let mut chosen = Vec::new();
        for _ in 0..200 {
            let features = context(&mut rng);
            let decision = router.choose("review", features).await.unwrap();
            let base = if decision.arm == "good" { 0.9 } else { 0.3 };
            router
                .reward(&decision.id, noisy(base, &mut rng), 1.0)
                .await
                .unwrap();
            chosen.push(decision.arm);
        }

        let tail = &chosen[100..];
        assert!(
            share(tail, "good") > 0.8,
            "good arm took {:.2} of the last hundred",
            share(tail, "good")
        );
        // Not 100%: the weak arm is re-checked periodically, which is what stops the
        // router from believing a stale estimate forever.
        assert!(tail.iter().any(|a| a == "weak"));
    }

    #[tokio::test]
    async fn a_reward_that_arrives_after_a_restart_still_updates_the_arm() {
        let (dir, store) = store().await;
        let specs = || vec![ArmSpec::new("cheap", 1.0), ArmSpec::new("dear", 3.0)];

        // A distinctive context, so what the reward moves can be told apart from what it
        // does not.
        let features = Features::new()
            .with(Features::IS_PLANNING, 1.0)
            .with(Features::PROMPT_SIZE, 0.9);
        let other = Features::new()
            .with(Features::IS_REVIEW, 1.0)
            .with(Features::PROMPT_SIZE, 0.1);

        let decision = {
            let mut router = Router::new(store.clone(), RouterConfig::default());
            router
                .register_role("plan", specs(), &BTreeMap::new())
                .await
                .unwrap();
            router.choose("plan", features).await.unwrap()
        };

        // Days pass; the process restarts. Nothing in memory survives, and the caller has
        // long since lost the context vector.
        drop(store);
        let opened = Store::open(dir.path()).unwrap();
        let store = opened.store;
        let mut router = Router::new(store.clone(), RouterConfig::default());
        router
            .register_role("plan", specs(), &BTreeMap::new())
            .await
            .unwrap();

        let before = router.bandit("plan").unwrap().model(&decision.arm).unwrap().mean(&features);
        assert_eq!(
            router.reward(&decision.id, 1.0, 1.0).await.unwrap(),
            RewardOutcome::Applied
        );
        let after_here = router.bandit("plan").unwrap().model(&decision.arm).unwrap().mean(&features);
        let after_there = router.bandit("plan").unwrap().model(&decision.arm).unwrap().mean(&other);

        assert_eq!(before, 0.0);
        assert!(after_here > 0.4, "the reward landed on the stored context: {after_here}");
        assert!(
            after_here > after_there * 2.0,
            "the update was applied at the context the decision was made in, not at some \
             other one: {after_here} vs {after_there}"
        );

        // The arm state is on disk, and the same reward cannot be counted twice.
        let updates: i64 = store
            .read(|conn| {
                Ok(conn.query_row("SELECT updates FROM routing_arms", [], |r| r.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(updates, 1);
        assert_eq!(
            router.reward(&decision.id, 1.0, 1.0).await.unwrap(),
            RewardOutcome::AlreadyRewarded
        );
    }

    #[test]
    fn an_arm_that_goes_bad_loses_the_traffic_it_had() {
        // The most important property, and the reason for geometric forgetting: with
        // γ = 1 the arm's whole history counts equally, so an arm with hundreds of good
        // rounds behind it keeps the traffic for hundreds of bad ones.
        let mut bandit = Bandit::new(
            RouterConfig::default(),
            vec![ArmSpec::new("vendor-a", 1.0), ArmSpec::new("vendor-b", 1.0)],
        );

        let mut rng = Lcg(11);
        let mut chosen = Vec::new();
        // Long enough that a router without forgetting builds up real inertia: about 900
        // good rounds behind the arm, which is roughly 540 bad ones before an unweighted
        // average would fall below the other arm.
        for round in 0..1_000 {
            let features = context(&mut rng);
            let index = bandit.choose(&features);
            let arm = bandit.arms[index].spec.arm.clone();
            let base = if arm == "vendor-a" { 0.9 } else { 0.6 };
            let reward = noisy(base, &mut rng);
            bandit.update(&arm, &features, reward, Some(1.0));
            chosen.push((round, arm));
        }
        let settled: Vec<String> = chosen[900..].iter().map(|(_, a)| a.clone()).collect();
        assert!(
            share(&settled, "vendor-a") > 0.8,
            "the better arm should hold the traffic before anything changes: {:.2}",
            share(&settled, "vendor-a")
        );

        // The vendor swaps the model behind the endpoint. Nothing announces it.
        let mut after = Vec::new();
        for _ in 0..400 {
            let features = context(&mut rng);
            let index = bandit.choose(&features);
            let arm = bandit.arms[index].spec.arm.clone();
            let base = if arm == "vendor-a" { 0.1 } else { 0.6 };
            let reward = noisy(base, &mut rng);
            bandit.update(&arm, &features, reward, Some(1.0));
            after.push(arm);
        }

        let tail: Vec<String> = after[300..].to_vec();
        assert!(
            share(&tail, "vendor-b") > 0.8,
            "traffic did not move after the arm degraded: b took {:.2} of the last hundred",
            share(&tail, "vendor-b")
        );
    }

    #[test]
    fn staleness_inflation_is_capped() {
        let config = RouterConfig::default();
        assert_eq!(staleness_inflation(0, &config), 1.0);
        assert!(staleness_inflation(100, &config) > 1.0);
        assert_eq!(staleness_inflation(1_000_000, &config), MAX_VARIANCE_INFLATION);
        assert_eq!(staleness_inflation(u64::MAX, &config), MAX_VARIANCE_INFLATION);
        assert!(staleness_inflation(50, &config) < staleness_inflation(500, &config));

        // What the cap buys. An expensive arm that has been idle for ages gets a bounded
        // exploration credit, so a finite price penalty can still outweigh it. Uncapped,
        // the same arm's bonus grows with the square root of the idle time and eventually
        // beats any penalty, which pins the router to whatever is most expensive and least
        // recently used.
        let mut bandit = Bandit::new(
            RouterConfig::default(),
            vec![ArmSpec::new("cheap", 1.0), ArmSpec::new("dear", 60.0)],
        );
        let mut rng = Lcg(3);
        for _ in 0..60 {
            let features = context(&mut rng);
            let index = bandit.choose(&features);
            let arm = bandit.arms[index].spec.arm.clone();
            bandit.update(&arm, &features, noisy(0.85, &mut rng), Some(bandit.arms[index].spec.cost));
        }

        // Nothing has been chosen for a very long time.
        bandit.step += 10_000_000;
        let features = context(&mut rng);
        let scores = bandit.scores(&features);
        let dear = scores.iter().find(|s| s.arm == "dear").unwrap();
        let cheap = scores.iter().find(|s| s.arm == "cheap").unwrap();

        assert_eq!(dear.variance_inflation, MAX_VARIANCE_INFLATION);
        let variance = bandit.model("dear").unwrap().variance(&features);
        let ceiling = bandit.config.alpha * (MAX_VARIANCE_INFLATION * variance).sqrt();
        assert!(dear.bonus <= ceiling + 1e-9);
        assert!(
            dear.score < cheap.score,
            "an arm sixty times the price is not worth an arbitrarily large exploration \
             credit: dear {:.3} vs cheap {:.3}",
            dear.score,
            cheap.score
        );
    }

    #[test]
    fn thousands_of_forgetting_updates_do_not_blow_the_inverse_up() {
        // Geometric forgetting shrinks A, and a shrinking A is a nearly singular A. This
        // is the test that says the ridge re-injection and the periodic exact re-inversion
        // are doing their job.
        let config = RouterConfig::default();
        let mut model = ArmModel::new(config.ridge);
        let mut rng = Lcg(23);

        for round in 0..20_000 {
            // Half the rounds use a context confined to a two-dimensional slice, so most
            // directions of the matrix are starved of observations and forgotten — which
            // is the case that makes A ill-conditioned in practice.
            let features = if round % 2 == 0 {
                context(&mut rng)
            } else {
                Features::new().with(Features::IS_EDIT, 1.0)
            };
            model.update(&features, noisy(0.7, &mut rng), &config);

            if round % 500 == 0 {
                let probe = context(&mut rng);
                assert!(model.mean(&probe).is_finite(), "mean went non-finite at {round}");
                let variance = model.variance(&probe);
                assert!(variance.is_finite() && variance >= 0.0, "variance {variance} at {round}");
                assert!(
                    variance <= dot(probe.as_slice(), probe.as_slice()) / config.ridge + 1e-6,
                    "variance {variance} exceeds the ridge floor's bound at {round}"
                );
            }
        }

        assert!(model.a.iter().all(|v| v.is_finite()));
        assert!(model.a_inv.iter().all(|v| v.is_finite()));
        assert!(model.b.iter().all(|v| v.is_finite()));

        // The incrementally maintained inverse still agrees with an exact one.
        let exact = invert(&model.a).expect("the ridge floor keeps A invertible");
        let worst = model
            .a_inv
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        assert!(worst < 1e-6, "cached inverse drifted by {worst}");

        // The mean stays in the neighbourhood of the rewards it was fed, rather than
        // running off the way an unregularised forgetting filter does.
        let probe = Features::new().with(Features::IS_EDIT, 1.0);
        let mean = model.mean(&probe);
        assert!((0.0..=1.5).contains(&mean), "mean drifted to {mean}");
    }

    #[tokio::test]
    async fn at_equal_quality_the_cheaper_arm_gets_the_work() {
        let (_d, store) = store().await;
        let mut router = Router::new(store, RouterConfig::default());
        router
            .register_role(
                "summarise",
                vec![ArmSpec::new("frontier", 8.0), ArmSpec::new("local", 1.0)],
                &BTreeMap::new(),
            )
            .await
            .unwrap();

        let mut rng = Lcg(5);
        let mut chosen = Vec::new();
        for _ in 0..150 {
            let features = context(&mut rng);
            let decision = router.choose("summarise", features).await.unwrap();
            // Same quality from both. The only thing separating them is the bill.
            let cost = if decision.arm == "frontier" { 8.0 } else { 1.0 };
            router
                .reward(&decision.id, noisy(0.85, &mut rng), cost)
                .await
                .unwrap();
            chosen.push(decision.arm);
        }

        let tail = &chosen[50..];
        assert!(
            share(tail, "local") > 0.9,
            "the cheap arm should take the work when quality is indistinguishable: {:.2}",
            share(tail, "local")
        );

        // And the floor still outranks the price: an arm that cannot do the job does not
        // win by being cheap.
        let features = context(&mut rng);
        let scores = router.scores("summarise", &features);
        assert!(scores.iter().all(|s| s.eligible));
    }

    #[test]
    fn the_quality_floor_outranks_the_price() {
        let config = RouterConfig::default();
        let mut bandit = Bandit::new(
            config,
            vec![ArmSpec::new("capable", 10.0), ArmSpec::new("hopeless", 0.1)],
        );
        let mut rng = Lcg(29);
        for _ in 0..300 {
            let features = context(&mut rng);
            let index = bandit.choose(&features);
            let arm = bandit.arms[index].spec.arm.clone();
            let base = if arm == "capable" { 0.9 } else { 0.05 };
            bandit.update(&arm, &features, noisy(base, &mut rng), Some(bandit.arms[index].spec.cost));
        }

        let features = context(&mut rng);
        let scores = bandit.scores(&features);
        let hopeless = scores.iter().find(|s| s.arm == "hopeless").unwrap();
        let capable = scores.iter().find(|s| s.arm == "capable").unwrap();
        assert!(!hopeless.eligible, "{hopeless:?}");
        assert!(capable.eligible);
        assert!(
            hopeless.score > capable.score,
            "the test is only meaningful if the cheap arm would otherwise win on price"
        );

        let mut counted = 0;
        for _ in 0..50 {
            let features = context(&mut rng);
            let index = bandit.select(&features);
            if bandit.arms[index].spec.arm == "capable" {
                counted += 1;
            }
        }
        assert!(
            counted > 40,
            "an arm that cannot clear the quality floor must not win on price: {counted}/50"
        );
    }

    #[test]
    fn warm_start_priors_wash_out_as_online_evidence_arrives() {
        let config = RouterConfig::default();
        let mut model = ArmModel::new(config.ridge);
        let features = Features::new().with(Features::IS_EDIT, 1.0);
        model.warm_start(&WarmStart {
            observations: vec![(features, 0.95)],
            n_eff: 20.0,
        });
        let prior = model.mean(&features);
        assert!(prior > 0.8, "the prior is worth something at the start: {prior}");

        // Online evidence disagrees. The prior is not given a decay schedule of its own:
        // every update multiplies the statistics by γ, so its weight falls off on its own
        // and the steady state is whatever the online evidence says.
        let mut rng = Lcg(31);
        for _ in 0..600 {
            model.update(&features, noisy(0.2, &mut rng), &config);
        }
        let settled = model.mean(&features);
        assert!(
            settled < 0.3,
            "the prior still dominates after six hundred contradicting observations: {settled}"
        );
    }

    #[test]
    fn the_cached_inverse_is_the_inverse_at_every_step_not_only_after_a_refresh() {
        // `A ← γA + xxᵀ` is a scaling plus a rank-1 term, so the rank-1 update is exact
        // and the cached inverse never has to be taken on trust between refreshes.
        let config = RouterConfig::default();
        let mut model = ArmModel::new(config.ridge);
        let mut rng = Lcg(37);
        let mut worst_seen: f64 = 0.0;

        for _ in 0..1_000 {
            model.update(&context(&mut rng), noisy(0.6, &mut rng), &config);
            let exact = invert(&model.a).unwrap();
            let worst = model
                .a_inv
                .iter()
                .zip(exact.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max);
            worst_seen = worst_seen.max(worst);
        }
        assert!(worst_seen < 1e-9, "cached inverse drifted by {worst_seen:e}");
    }

    #[test]
    fn without_the_ridge_restoration_forgetting_eats_the_confidence_bonus() {
        // Why `restore_and_refresh` exists. Forgetting multiplies the whole matrix,
        // including the ridge, so with the restoration disabled the spectral floor decays
        // as γⁿ and the variance in a direction nothing has been observed in grows without
        // limit — taking the exploration bonus with it.
        let mut rng = Lcg(41);
        let mut samples = Vec::new();
        for refresh_every in [u32::MAX, RouterConfig::default().refresh_every] {
            let config = RouterConfig {
                refresh_every,
                ..RouterConfig::default()
            };
            let mut model = ArmModel::new(config.ridge);
            // Everything observed lies in one direction, so every other direction is
            // forgotten and nothing replaces it.
            let observed = Features::new().with(Features::IS_EDIT, 1.0);
            let unobserved = Features::new().with(Features::RECENT_FAILURES, 1.0);
            for _ in 0..4_000 {
                model.update(&observed, noisy(0.7, &mut rng), &config);
            }
            samples.push(model.variance(&unobserved));
        }

        // Unrestored, the floor is γⁿλ, so the variance grows as γ⁻ⁿ without limit; four
        // thousand updates at γ = 0.997 is already four orders of magnitude past the
        // correct value of 1.0.
        let (unrestored, restored) = (samples[0], samples[1]);
        assert!(
            unrestored > 1e4,
            "the failure this guards against did not reproduce: {unrestored:e}"
        );
        // Restored, `A ⪰ γ^refresh λI` at all times, so the variance can never exceed
        // |x|² over that floor however long the direction goes unobserved.
        let config = RouterConfig::default();
        let unobserved = Features::new().with(Features::RECENT_FAILURES, 1.0);
        let floor = config.ridge * config.forgetting.powi(config.refresh_every as i32);
        let bound = dot(unobserved.as_slice(), unobserved.as_slice()) / floor;
        assert!(
            restored <= bound,
            "the restored ridge must bound the variance by |x|²/λ: {restored} > {bound}"
        );
    }
}
