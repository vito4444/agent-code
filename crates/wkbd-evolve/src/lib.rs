//! Self-improvement, kept on a short leash.
//!
//! Four things the workbench does to get better at its job, and the reason each one is
//! shaped the way it is:
//!
//! - [`playbook`] — accumulated working notes. Itemized bullets merged by ordinary code,
//!   never a document a model rewrites, because whole-context rewriting has a measured
//!   collapse mode that ends up worse than not adapting at all.
//! - [`proposals`] — everything the system writes for its own future consumption passes
//!   through a human, and the approval binds to the bytes rather than to a name or an id.
//! - [`routing`] — which model does which job, as a cost-control mechanism under a
//!   quality floor rather than as a quality-improvement mechanism.
//! - [`distill`] — turning repeated, externally verified success into a reusable
//!   procedure, which then has to go through [`proposals`] like everything else.
//!
//! Two rules hold across all four:
//!
//! **Anything a model contributes arrives through a trait with a deterministic default
//! implementation.** The default implementations are not mocks; they are what runs when no
//! model is configured, which is also what makes the tests here run without one.
//!
//! **The model never gets the last word on a state transition.** It can propose bullets
//! ([`playbook::Reflector`]) and it can name things ([`distill::StepNamer`]); merging,
//! pruning, gating, approval and scoring are all ordinary code.

pub mod distill;
pub mod playbook;
pub mod proposals;
pub mod routing;

pub use playbook::{Bullet, PlaybookDelta, SourceTrust};
pub use proposals::{Proposal, ProposalPayload, Risk};
pub use routing::{Decision, Features, Router};
