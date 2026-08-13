//! Driving agent processes over ACP.
//!
//! Two structural decisions live here and both are load-bearing.
//!
//! **The process pool is keyed by agent *plus every setting that can only be applied when
//! the process starts.*** Keying by agent alone works right up until the day a model
//! selector is added, at which point changing the model means restarting the one process
//! every session for that agent is sharing. The protocol does support changing some
//! settings at runtime — `session/set_config_option` with a `model` or `thought_level`
//! category — but support is per-agent and has to be probed, so the pool must be able to
//! express "these two sessions cannot share a process" from the start.
//!
//! **[`SessionFactory`] is the only way to open a session.** Rules and recalled memory are
//! injected there. A new chat, a resumed conversation, a model switch that has to start a
//! fresh session, an orchestrator handing work to a worker, and a replay are all separate
//! entry points, and each one is somewhere the injection could be forgotten. Making them
//! all go through one constructor turns "remember to add it" into a type error.

pub mod conn;
pub mod pool;
pub mod probe;
pub mod session;
pub mod wire;

pub use conn::{Connection, Direction, Incoming, RawFrame, RpcError};
pub use pool::{AgentPool, AgentSpec, LaunchConfig, ProcessKey};
pub use probe::{CapabilityReport, ConfigSupport, probe_agent};
pub use session::{
    Prelude, PreludeProvider, PromptOutcome, SessionFactory, SessionHandle,
    SessionOpenRequest, SessionPurpose,
};

#[cfg(test)]
mod tests;
