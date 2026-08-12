//! Wire-adjacent types and the normalization layer.
//!
//! The crate boundary is deliberate: `wkbd-proto` knows about ACP and about our internal
//! event model, and nothing else in the workspace is allowed to depend on ACP directly.
//! That keeps the "what does a future protocol version break" question answerable by
//! reading one crate.

pub mod event;
pub mod normalize;
pub mod turn;

pub use event::*;
pub use event::{run_stream_id, RunEvent, RunStatus, TaskStatus, TaskSummary};
pub use normalize::{max_concurrent_live, Normalizer, RawUpdate};
pub use turn::{
    FileAccessRecord, SegmentView, ToolCallView, TurnItem, TurnView, ViewBuilder,
};

#[cfg(test)]
mod tests;
