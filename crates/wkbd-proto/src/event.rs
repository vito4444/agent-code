//! The internal normalized event model.
//!
//! Everything downstream of the ACP wire — the event log, the UI, memory extraction,
//! the orchestrator's stuck detection — reads *this* model, never raw ACP. Two reasons:
//!
//! 1. ACP v1 makes `messageId` optional on content chunks, so raw chunks cannot be
//!    reliably grouped into messages. We resolve grouping exactly once, here, and
//!    everything downstream gets a guaranteed-present `SegmentId`.
//! 2. ACP explicitly documents that its `sessionUpdate` variant set is not exhaustive.
//!    An unknown variant must not be able to reach the UI as an unhandled case.

use serde::{Deserialize, Serialize};

/// Identifies one contiguous run of chunks that belong to the same logical message.
///
/// When the agent supplies `messageId` we use it verbatim. When it does not, we
/// synthesize one; `synthesized` records which happened, because the two cases have
/// very different reliability and the UI is allowed to say so.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId {
    pub raw: String,
    pub synthesized: bool,
}

impl SegmentId {
    pub fn from_agent(id: impl Into<String>) -> Self {
        Self { raw: id.into(), synthesized: false }
    }

    pub fn synthesized(turn: u64, ordinal: u64) -> Self {
        Self { raw: format!("syn:{turn}:{ordinal}"), synthesized: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    /// The agent's internal reasoning. Rendered in the collapsible "thought process" band.
    Thought,
    /// The agent's answer text. Rendered in the highest-contrast band.
    Message,
    /// Echo of the user's own message, when the agent chooses to echo it.
    UserEcho,
}

/// Lifecycle of a segment.
///
/// `Live` is the single most load-bearing piece of state in the chat UI: it is what
/// drives auto-expansion. The invariant enforced by [`Normalizer`] is that at most one
/// segment is `Live` at any point in the stream — which is what stops a turn with six
/// thought segments from expanding all six and pushing the answer off screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Live,
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    /// Not an ACP v1 status. Synthesized by us when a turn is cancelled while the call
    /// was still open, because ACP says the client SHOULD surface those as cancelled.
    Cancelled,
}

/// Mirrors ACP `ToolKind`, plus `Unknown` for forward compatibility. We keep our own
/// enum rather than re-exporting so that an unrecognized kind from a future protocol
/// version degrades to a generic card instead of failing to deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
    Unknown(String),
}

/// Content attached to a tool call. `Diff` and `Terminal` exist as first-class variants
/// because the whole point of the tool-call card is that a diff renders as a diff and
/// terminal output renders as a terminal, rather than both collapsing into grey text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text { text: String },
    Diff { path: String, old_text: Option<String>, new_text: String },
    Terminal { terminal_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLocation {
    pub path: String,
    pub line: Option<u32>,
}

/// One option offered in a permission request.
///
/// `kind` is only a hint from the agent about how to draw the button. Our own
/// remembering policy is keyed on (tool kind, content hash) and lives client-side —
/// see `wkbd-sec::permission`. ACP deliberately provides no binding between a
/// permission option and a category of operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Unknown,
}

/// A session configuration option as surfaced to the UI.
///
/// The UI never hardcodes a model name. It renders whatever the agent declared, and
/// draws no control at all when the agent declared nothing — an empty menu tells the
/// user "there is a choice here and it is broken", which is worse than no menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOptionView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `model` / `model_config` / `thought_level` / `mode` / other. Optional per spec;
    /// unknown values must degrade to a plain select rather than being dropped.
    pub category: Option<String>,
    pub value: ConfigValueView,
    /// False when this option can only be set at process spawn time. Derived from the
    /// capability probe, not from the protocol: agents that expose the option over
    /// `session/set_config_option` are switchable in place, agents that only read argv
    /// or env are not, and the UI must say which.
    pub live_switchable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigValueView {
    Select { current: String, options: Vec<ConfigChoice> },
    Boolean { current: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// The agent ended the turn with a reason we do not recognize. Kept distinct from
    /// `EndTurn` so the UI never claims a clean finish it cannot substantiate.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntryView {
    pub content: String,
    pub priority: String,
    pub status: String,
}

// Tagged with `event` rather than `kind`: several variants carry their own `kind` field
// (a tool call's ToolKind, a segment's SegmentKind) and an internal tag may not collide
// with a variant field name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventPayload {
    TurnStarted { turn: u64, prompt: String },
    TurnEnded { turn: u64, stop_reason: StopReason },

    SegmentStarted { segment: SegmentId, kind: SegmentKind },
    SegmentChunk { segment: SegmentId, text: String },
    SegmentSettled { segment: SegmentId },

    ToolCallStarted {
        tool_call_id: String,
        title: String,
        kind: ToolKind,
        status: ToolStatus,
    },
    ToolCallUpdated {
        tool_call_id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        content: Vec<ToolContent>,
        locations: Vec<ToolLocation>,
    },

    PermissionRequested {
        request_id: String,
        tool_call_id: Option<String>,
        title: String,
        options: Vec<PermissionOption>,
    },
    PermissionResolved {
        request_id: String,
        option_id: Option<String>,
        /// True when our own policy answered without asking the user.
        auto: bool,
    },

    ConfigOptionsChanged { options: Vec<ConfigOptionView> },

    /// Only ever emitted when the agent actually reported usage. We never estimate:
    /// as an ACP client we cannot see the agent's system prompt, loaded rule files or
    /// tool schemas, so a locally computed number would be a different quantity wearing
    /// the same label.
    UsageChanged { used: u64, size: u64, cost: Option<CostView> },

    PlanChanged { entries: Vec<PlanEntryView> },

    /// A `sessionUpdate` variant we do not model. Recorded rather than dropped so that
    /// the raw-message inspector can show what we ignored, and so a future variant
    /// becoming load-bearing shows up as data instead of silence.
    UnknownUpdate { discriminant: String, raw: String },

    AgentError { message: String },
    /// Emitted by the supervisor, not the agent.
    AgentExited { code: Option<i32>, signal: Option<i32> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostView {
    pub amount: f64,
    pub currency: String,
}

/// An event as it exists in the log. `seq` is assigned by the database inside the write
/// transaction and is never produced by an in-memory counter: a counter restarts at
/// zero after a crash, and the resulting primary-key collision would make the app
/// permanently unopenable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub session_id: String,
    pub at_ms: i64,
    pub payload: EventPayload,
}

/// An event that has been produced but not yet persisted, hence has no `seq` yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingEvent {
    pub session_id: String,
    pub payload: EventPayload,
}

impl PendingEvent {
    pub fn new(session_id: impl Into<String>, payload: EventPayload) -> Self {
        Self { session_id: session_id.into(), payload }
    }
}
