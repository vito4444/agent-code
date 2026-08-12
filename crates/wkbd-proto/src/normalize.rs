//! Turn segmentation: raw ACP `session/update` notifications in, normalized events out.
//!
//! This is the only place in the system that decides where one thought ends and the next
//! begins. It exists because a real agent thinks repeatedly *between* tool calls, so a
//! single turn produces several thought segments. Anything that treats "the turn has not
//! ended" as "this thought is in progress" will expand every one of them at once and push
//! the answer off screen.
//!
//! Two rules do the work:
//!
//! - Grouping is by `messageId` when the agent sends one. ACP: "All chunks belonging to
//!   the same message share the same `messageId`. A change in `messageId` indicates a new
//!   message has started." In v1 the field is optional, so there is a fallback: any
//!   interleaving update of a different kind (a tool call, or text of the other kind)
//!   closes the open segment.
//! - Opening a segment always closes the previously open one first. That yields the
//!   invariant the UI depends on: **at most one segment is `Live` at any point in the
//!   stream.** Auto-expansion can then be "expand iff Live" with no further logic.

use crate::event::*;

/// Kinds of raw update the normalizer accepts. Deliberately not ACP's own enum: the
/// adapter layer (`wkbd-agent`) is responsible for mapping ACP — including unknown
/// variants — onto this, so that the segmentation logic is testable without a protocol
/// stack and so an unmodelled ACP variant has exactly one place to land.
#[derive(Debug, Clone, PartialEq)]
pub enum RawUpdate {
    TextChunk { kind: SegmentKind, message_id: Option<String>, text: String },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: ToolKind,
        status: ToolStatus,
        content: Vec<ToolContent>,
        locations: Vec<ToolLocation>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        content: Vec<ToolContent>,
        locations: Vec<ToolLocation>,
    },
    Plan { entries: Vec<PlanEntryView> },
    ConfigOptions { options: Vec<ConfigOptionView> },
    Usage { used: u64, size: u64, cost: Option<CostView> },
    Unknown { discriminant: String, raw: String },
}

#[derive(Debug, Clone, PartialEq)]
struct OpenSegment {
    id: SegmentId,
    kind: SegmentKind,
}

/// Per-session segmentation state.
#[derive(Debug, Default)]
pub struct Normalizer {
    turn: u64,
    in_turn: bool,
    open: Option<OpenSegment>,
    /// Counter for synthesized ids within the current turn. Only used for agents that
    /// omit `messageId`; reset per turn so ids stay short and readable in the log.
    syn_ordinal: u64,
    /// Tool calls seen open in this turn, so a cancellation can settle them as cancelled
    /// rather than leaving them spinning forever.
    open_tool_calls: Vec<String>,
    /// Whether any chunk in this session arrived without a `messageId`. Surfaced to the
    /// UI so it can mark segmentation as best-effort for this agent instead of implying
    /// the same fidelity as an agent that supplies ids.
    saw_missing_message_id: bool,
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current_turn(&self) -> u64 {
        self.turn
    }

    pub fn segmentation_is_best_effort(&self) -> bool {
        self.saw_missing_message_id
    }

    /// True while a segment is open. Used by tests and by the stuck detector.
    pub fn has_live_segment(&self) -> bool {
        self.open.is_some()
    }

    pub fn begin_turn(&mut self, prompt: impl Into<String>) -> Vec<EventPayload> {
        let mut out = Vec::new();
        // Defensive: a second `begin_turn` without an intervening end would otherwise
        // leave the previous turn's segment live forever.
        out.extend(self.settle_open());
        self.turn += 1;
        self.in_turn = true;
        self.syn_ordinal = 0;
        self.open_tool_calls.clear();
        out.push(EventPayload::TurnStarted { turn: self.turn, prompt: prompt.into() });
        out
    }

    /// Ends the turn.
    ///
    /// The only authoritative signal that a turn is over is the `session/prompt` response
    /// returning with a stop reason. We never infer it from the stream going quiet: doing
    /// so is what produces "queued message delivered at the next LLM pause instead of at
    /// end of turn" bugs.
    pub fn end_turn(&mut self, stop_reason: StopReason) -> Vec<EventPayload> {
        let mut out = Vec::new();
        out.extend(self.settle_open());
        if stop_reason == StopReason::Cancelled {
            for id in std::mem::take(&mut self.open_tool_calls) {
                out.push(EventPayload::ToolCallUpdated {
                    tool_call_id: id,
                    title: None,
                    status: Some(ToolStatus::Cancelled),
                    content: Vec::new(),
                    locations: Vec::new(),
                });
            }
        }
        self.open_tool_calls.clear();
        self.in_turn = false;
        out.push(EventPayload::TurnEnded { turn: self.turn, stop_reason });
        out
    }

    pub fn push(&mut self, update: RawUpdate) -> Vec<EventPayload> {
        match update {
            RawUpdate::TextChunk { kind, message_id, text } => {
                self.text_chunk(kind, message_id, text)
            }
            RawUpdate::ToolCall { tool_call_id, title, kind, status, content, locations } => {
                // A tool call is a hard segment boundary. For agents that supply
                // `messageId` this is redundant; for agents that do not, it is the only
                // signal we get that the thought before it has finished.
                let mut out = self.settle_open();
                if !matches!(status, ToolStatus::Completed | ToolStatus::Failed) {
                    self.open_tool_calls.push(tool_call_id.clone());
                }
                out.push(EventPayload::ToolCallStarted {
                    tool_call_id: tool_call_id.clone(),
                    title,
                    kind,
                    status,
                });
                if !content.is_empty() || !locations.is_empty() {
                    out.push(EventPayload::ToolCallUpdated {
                        tool_call_id,
                        title: None,
                        status: None,
                        content,
                        locations,
                    });
                }
                out
            }
            RawUpdate::ToolCallUpdate { tool_call_id, title, status, content, locations } => {
                let mut out = self.settle_open();
                if let Some(s) = status {
                    if matches!(s, ToolStatus::Completed | ToolStatus::Failed | ToolStatus::Cancelled)
                    {
                        self.open_tool_calls.retain(|t| t != &tool_call_id);
                    } else if !self.open_tool_calls.contains(&tool_call_id) {
                        self.open_tool_calls.push(tool_call_id.clone());
                    }
                }
                out.push(EventPayload::ToolCallUpdated {
                    tool_call_id,
                    title,
                    status,
                    content,
                    locations,
                });
                out
            }
            RawUpdate::Plan { entries } => vec![EventPayload::PlanChanged { entries }],
            RawUpdate::ConfigOptions { options } => {
                vec![EventPayload::ConfigOptionsChanged { options }]
            }
            RawUpdate::Usage { used, size, cost } => {
                vec![EventPayload::UsageChanged { used, size, cost }]
            }
            RawUpdate::Unknown { discriminant, raw } => {
                vec![EventPayload::UnknownUpdate { discriminant, raw }]
            }
        }
    }

    fn text_chunk(
        &mut self,
        kind: SegmentKind,
        message_id: Option<String>,
        text: String,
    ) -> Vec<EventPayload> {
        let mut out = Vec::new();

        let target = match message_id {
            Some(m) => SegmentId::from_agent(m),
            None => {
                self.saw_missing_message_id = true;
                match &self.open {
                    // Same kind and nothing interleaved since: this is a continuation.
                    Some(open) if open.kind == kind => open.id.clone(),
                    _ => {
                        self.syn_ordinal += 1;
                        SegmentId::synthesized(self.turn, self.syn_ordinal)
                    }
                }
            }
        };

        let needs_open = match &self.open {
            Some(open) => open.id != target,
            None => true,
        };

        if needs_open {
            out.extend(self.settle_open());
            self.open = Some(OpenSegment { id: target.clone(), kind });
            out.push(EventPayload::SegmentStarted { segment: target.clone(), kind });
        }

        out.push(EventPayload::SegmentChunk { segment: target, text });
        out
    }

    fn settle_open(&mut self) -> Vec<EventPayload> {
        match self.open.take() {
            Some(open) => vec![EventPayload::SegmentSettled { segment: open.id }],
            None => Vec::new(),
        }
    }
}

/// Replays a payload stream and returns the number of segments that are simultaneously
/// live at the high-water mark. Used by tests to assert the core invariant; also used by
/// the daemon's self-check on startup against a recorded cassette.
pub fn max_concurrent_live(payloads: &[EventPayload]) -> usize {
    let mut live: Vec<&SegmentId> = Vec::new();
    let mut max = 0;
    for p in payloads {
        match p {
            EventPayload::SegmentStarted { segment, .. } => {
                if !live.iter().any(|s| *s == segment) {
                    live.push(segment);
                }
                max = max.max(live.len());
            }
            EventPayload::SegmentSettled { segment } => {
                live.retain(|s| *s != segment);
            }
            _ => {}
        }
    }
    max
}
