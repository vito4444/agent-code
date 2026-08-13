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

/// What the agent said it can be given in a prompt.
///
/// Text and resource links are the protocol's baseline and every agent must accept them, so
/// they are not represented here — there is nothing to negotiate. The three that are here are
/// the ones an agent may not support, and each one that is false has to remove a control from
/// the composer rather than degrade quietly: a prompt carrying a content block the agent never
/// advertised is a protocol violation, and the agents that do not simply error are worse,
/// because they drop the block and answer as if the user attached nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

/// How an attachment actually reached the agent.
///
/// Recorded, and shown, because the two are not interchangeable. `Embedded` means the agent was
/// handed the bytes and cannot fail to see them. `Link` means it was handed a path and has to go
/// and read it — which it may lack the tools, the permission or the inclination to do. An
/// attachment that silently became a link is the failure mode worth naming: the user believes
/// the file is in the conversation, the agent never opened it, and the answer that comes back
/// looks like the model ignoring instructions rather than a capability gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SentAs {
    Embedded,
    Image,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// `file:///absolute/path`, the URI the agent was given.
    pub uri: String,
    /// Relative to the project root, which is what the user typed and can recognize.
    pub name: String,
    pub sent_as: SentAs,
    pub bytes: Option<u64>,
    /// Why it went as a link when it could have been embedded: `too-large`, `not-text`,
    /// `directory`, `agent-cannot-embed`. `None` when nothing was given up.
    pub degraded: Option<String>,
}

// Tagged with `event` rather than `kind`: several variants carry their own `kind` field
// (a tool call's ToolKind, a segment's SegmentKind) and an internal tag may not collide
// with a variant field name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventPayload {
    /// `attachments` defaults so that turns recorded before mentions existed still read back.
    TurnStarted {
        turn: u64,
        prompt: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
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
    /// The turn ended while this request was still unanswered.
    ///
    /// Its own event rather than a resolution with no option, because "nobody answered and the agent
    /// stopped waiting" and "somebody refused" are different things that happened, and only one of
    /// them is a decision. Recording the lapse as a refusal would put a choice in the log that no
    /// person made, which is exactly the kind of entry a later reader has no way to question.
    PermissionExpired { request_id: String },

    ConfigOptionsChanged { options: Vec<ConfigOptionView> },

    /// Only ever emitted when the agent actually reported usage. We never estimate:
    /// as an ACP client we cannot see the agent's system prompt, loaded rule files or
    /// tool schemas, so a locally computed number would be a different quantity wearing
    /// the same label.
    UsageChanged { used: u64, size: u64, cost: Option<CostView> },

    /// The agent's plan for this turn, replacing whatever it last said about this plan id.
    ///
    /// Replaced wholesale rather than merged, because the protocol requires the agent to send every
    /// entry on every update. Merging would keep an entry the agent had dropped.
    PlanChanged { plan_id: String, entries: Vec<PlanEntryView> },

    /// A `sessionUpdate` variant we do not model. Recorded rather than dropped so that
    /// the raw-message inspector can show what we ignored, and so a future variant
    /// becoming load-bearing shows up as data instead of silence.
    UnknownUpdate { discriminant: String, raw: String },

    AgentError { message: String },
    /// Emitted by the supervisor, not the agent.
    AgentExited { code: Option<i32>, signal: Option<i32> },

    /// An orchestration event, nested rather than flattened so the two vocabularies cannot
    /// collide as they grow.
    Run { run: RunEvent },

    /// The agent asked us to read or write a file on its behalf.
    ///
    /// Recorded for every attempt, allowed or refused. The protocol has the client perform real
    /// disk I/O with an absolute path the agent chose and defines no boundary of its own, which
    /// makes this the shortest route around every other check in the system. Enforcement that
    /// leaves no record is enforcement nobody can audit, and a refusal is often the most
    /// interesting thing in a transcript.
    FileAccess {
        op: FileOp,
        /// What the agent asked for, verbatim, before any resolution.
        requested: String,
        /// Where it actually resolved to, when it was allowed.
        resolved: Option<String>,
        allowed: bool,
        /// A stable classification when refused: `outside-root`, `symlink-encountered`,
        /// `parent-traversal` and so on.
        refusal: Option<String>,
        bytes: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Read,
    Write,
}

/// Orchestration events.
///
/// These share the event log with conversation events rather than living in their own table. The
/// `session_id` column is really a stream identifier, and a run uses `run:<uuid>`. One log means
/// one resume mechanism, one ordering, and one answer to "what happened, in what order" — which is
/// the whole reason a run can be replayed at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    Started {
        run_id: String,
        goal: String,
        project_root: String,
        base_commit: String,
    },
    /// The planner produced a graph and it survived validation.
    Planned {
        tasks: Vec<TaskSummary>,
        /// Task ids grouped so that everything in one group can run at once.
        waves: Vec<Vec<String>>,
        attempt: u32,
    },
    /// The planner produced a graph that did not survive validation.
    ///
    /// Recorded rather than retried silently: a planner that keeps producing invalid graphs is
    /// something the reader needs to see, and the problems are the input to the next attempt.
    PlanRejected {
        problems: Vec<String>,
        attempt: u32,
    },
    TaskStateChanged {
        task_id: String,
        status: TaskStatus,
        /// Why, when the status alone does not say. A failure reason, a conflict summary.
        detail: Option<String>,
    },
    /// A task's isolated workspace exists and starts from this commit.
    ///
    /// The commit is the interesting part: for a task with dependencies it is a real merge of their
    /// results, which is what makes the edge carry the work rather than a description of it.
    TaskWorkspaceReady {
        task_id: String,
        branch: String,
        start_commit: String,
        /// The dependency commits folded into the starting point, in order.
        from_dependencies: Vec<String>,
    },
    TaskVerified {
        task_id: String,
        passed: bool,
        /// Assertions that were supposed to start passing and did not.
        missing_pass: Vec<String>,
        /// Assertions that were passing and stopped.
        regressed: Vec<String>,
        detail: Option<String>,
    },
    /// A deterministic predicate said the graph was wrong.
    Replanning {
        trigger: String,
        task_id: String,
        attempt: u32,
    },
    /// Everything that passed has been combined, and the result is waiting for a human.
    ///
    /// Deliberately not an automatic merge. Verification proves the tests we named pass; it does
    /// not prove the change is what was wanted, and the published rates at which models exploit
    /// weak test suites are high enough that the gate stays closed until somebody looks.
    AwaitingMerge {
        commit: String,
        order: Vec<String>,
        /// Tasks that never made it into the candidate.
        excluded: Vec<String>,
    },
    /// A candidate could not be assembled because entries would not combine.
    MergeRejected {
        task_id: String,
        detail: String,
        /// The tasks that did combine, which keep their work.
        merged: Vec<String>,
    },
    Finished {
        status: RunStatus,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub depends_on: Vec<String>,
    pub declared_paths: Vec<String>,
    /// The command the acceptance check will run.
    pub verify_cmd: String,
    /// Assertions that decide whether the task succeeded. A task with none of these would have
    /// been rejected at validation; they are shown so the reader can see what "done" means.
    pub must_pass: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Dispatched,
    Verifying,
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Planning,
    Running,
    /// Waiting for a human to merge.
    AwaitingMerge,
    Done,
    Failed,
    Cancelled,
}

/// The stream identifier a run's events are recorded under.
pub fn run_stream_id(run_id: &str) -> String {
    format!("run:{run_id}")
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
