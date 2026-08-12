//! Materializes the event log into the view the chat UI renders.
//!
//! This lives in Rust rather than in the UI for one reason: it is the thing that must be
//! identical between a live stream and a replayed log. If the UI folded events into a
//! view itself, "what did that run actually look like" would depend on which code path
//! produced it, and a replayed run could render differently from the original.

use crate::event::*;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SegmentView {
    pub id: SegmentId,
    pub kind: SegmentKind,
    pub text: String,
    pub state: SegmentState,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallView {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub content: Vec<ToolContent>,
    pub locations: Vec<ToolLocation>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnItem {
    Segment(SegmentView),
    ToolCall(ToolCallView),
    Permission {
        request_id: String,
        title: String,
        options: Vec<PermissionOption>,
        resolved_with: Option<String>,
        auto: bool,
    },
    Error {
        message: String,
    },
    /// A file access performed on the agent's behalf, allowed or refused.
    ///
    /// In the turn, in order, rather than only in the audit list. Both halves of that matter and the
    /// first version got both wrong by keeping allowed accesses out of the transcript entirely.
    ///
    /// An allowed write is the agent's work. For an agent that edits through the protocol's file
    /// methods — the path we encourage, because it is the only one that is bounded and logged — the
    /// transcript otherwise shows a thought and an answer and no sign that any file changed.
    ///
    /// A refusal is a boundary decision rather than a fault, and it is the single most interesting
    /// line a transcript can contain. Enforcement the user cannot see is enforcement they cannot
    /// audit.
    File {
        op: FileOp,
        requested: String,
        resolved: Option<String>,
        allowed: bool,
        refusal: Option<String>,
        bytes: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileAccessRecord {
    pub op: FileOp,
    pub requested: String,
    pub resolved: Option<String>,
    pub allowed: bool,
    pub refusal: Option<String>,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnView {
    pub turn: u64,
    pub prompt: String,
    pub items: Vec<TurnItem>,
    pub stop_reason: Option<StopReason>,
    /// True when any chunk in this turn lacked an agent-supplied `messageId`, i.e. the
    /// thought/answer split below is our best effort rather than the agent's own.
    pub segmentation_best_effort: bool,
}

impl TurnView {
    /// The segment the UI should auto-expand. There is at most one by construction of
    /// [`crate::Normalizer`]; returning an `Option` rather than a list makes it impossible
    /// for a caller to accidentally expand several.
    pub fn live_segment(&self) -> Option<&SegmentView> {
        self.items.iter().rev().find_map(|item| match item {
            TurnItem::Segment(s) if s.state == SegmentState::Live => Some(s),
            _ => None,
        })
    }
}

/// Folds a payload stream into turns.
#[derive(Debug, Default)]
pub struct ViewBuilder {
    turns: Vec<TurnView>,
    /// segment id -> (turn index, item index)
    seg_index: HashMap<String, (usize, usize)>,
    tool_index: HashMap<String, (usize, usize)>,
    perm_index: HashMap<String, (usize, usize)>,
    pub latest_usage: Option<(u64, u64, Option<CostView>)>,
    pub config_options: Vec<ConfigOptionView>,
    pub plan: Vec<PlanEntryView>,
    pub unknown_updates: Vec<(String, String)>,
    /// Every file access attempted on the agent's behalf, allowed or not. The audit trail for
    /// the one interface where an unsandboxed process acts for a sandboxed one.
    pub file_accesses: Vec<FileAccessRecord>,
    /// Orchestration events, which arrive on their own stream and are collected rather than
    /// folded into turns: a run is not a conversation and forcing it into one would misrepresent
    /// both.
    pub run_events: Vec<RunEvent>,
}

impl ViewBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn turns(&self) -> &[TurnView] {
        &self.turns
    }

    pub fn into_turns(self) -> Vec<TurnView> {
        self.turns
    }

    pub fn apply_all(&mut self, payloads: &[EventPayload]) {
        for p in payloads {
            self.apply(p);
        }
    }

    pub fn apply(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::TurnStarted { turn, prompt } => {
                self.turns.push(TurnView {
                    turn: *turn,
                    prompt: prompt.clone(),
                    items: Vec::new(),
                    stop_reason: None,
                    segmentation_best_effort: false,
                });
            }
            EventPayload::TurnEnded { stop_reason, .. } => {
                if let Some(t) = self.turns.last_mut() {
                    t.stop_reason = Some(*stop_reason);
                    // Belt and braces: a turn must never end with a live segment. If the
                    // normalizer were bypassed (a hand-written cassette, say) this keeps
                    // the UI from leaving a spinner running forever.
                    for item in t.items.iter_mut() {
                        if let TurnItem::Segment(s) = item {
                            s.state = SegmentState::Settled;
                        }
                    }
                }
            }
            EventPayload::SegmentStarted { segment, kind } => {
                if let Some(ti) = self.turns.len().checked_sub(1) {
                    // Reopening a previously settled segment is legitimate: it is what an
                    // agent does when it returns to the same `messageId` after a tool call.
                    if let Some(&(t, i)) = self.seg_index.get(&segment.raw) {
                        if let Some(TurnItem::Segment(s)) =
                            self.turns.get_mut(t).and_then(|tv| tv.items.get_mut(i))
                        {
                            s.state = SegmentState::Live;
                            return;
                        }
                    }
                    let turn = &mut self.turns[ti];
                    turn.items.push(TurnItem::Segment(SegmentView {
                        id: segment.clone(),
                        kind: *kind,
                        text: String::new(),
                        state: SegmentState::Live,
                    }));
                    if segment.synthesized {
                        turn.segmentation_best_effort = true;
                    }
                    self.seg_index.insert(segment.raw.clone(), (ti, turn.items.len() - 1));
                }
            }
            EventPayload::SegmentChunk { segment, text } => {
                if let Some(&(t, i)) = self.seg_index.get(&segment.raw) {
                    if let Some(TurnItem::Segment(s)) =
                        self.turns.get_mut(t).and_then(|tv| tv.items.get_mut(i))
                    {
                        s.text.push_str(text);
                    }
                }
            }
            EventPayload::SegmentSettled { segment } => {
                if let Some(&(t, i)) = self.seg_index.get(&segment.raw) {
                    if let Some(TurnItem::Segment(s)) =
                        self.turns.get_mut(t).and_then(|tv| tv.items.get_mut(i))
                    {
                        s.state = SegmentState::Settled;
                    }
                }
            }
            EventPayload::ToolCallStarted { tool_call_id, title, kind, status } => {
                if let Some(ti) = self.turns.len().checked_sub(1) {
                    let turn = &mut self.turns[ti];
                    turn.items.push(TurnItem::ToolCall(ToolCallView {
                        tool_call_id: tool_call_id.clone(),
                        title: title.clone(),
                        kind: kind.clone(),
                        status: *status,
                        content: Vec::new(),
                        locations: Vec::new(),
                    }));
                    self.tool_index.insert(tool_call_id.clone(), (ti, turn.items.len() - 1));
                }
            }
            EventPayload::ToolCallUpdated {
                tool_call_id,
                title,
                status,
                content,
                locations,
            } => {
                // Upsert: ACP v2 makes the first update create the call, and v1 agents can
                // send an update for a call we never saw a `tool_call` for. Treat a missing
                // parent as "create it" rather than dropping the update.
                if !self.tool_index.contains_key(tool_call_id) {
                    if let Some(ti) = self.turns.len().checked_sub(1) {
                        let turn = &mut self.turns[ti];
                        turn.items.push(TurnItem::ToolCall(ToolCallView {
                            tool_call_id: tool_call_id.clone(),
                            title: title.clone().unwrap_or_else(|| tool_call_id.clone()),
                            kind: ToolKind::Other,
                            status: status.unwrap_or(ToolStatus::InProgress),
                            content: Vec::new(),
                            locations: Vec::new(),
                        }));
                        self.tool_index
                            .insert(tool_call_id.clone(), (ti, turn.items.len() - 1));
                    }
                }
                if let Some(&(t, i)) = self.tool_index.get(tool_call_id) {
                    if let Some(TurnItem::ToolCall(tc)) =
                        self.turns.get_mut(t).and_then(|tv| tv.items.get_mut(i))
                    {
                        if let Some(title) = title {
                            tc.title = title.clone();
                        }
                        if let Some(status) = status {
                            tc.status = *status;
                        }
                        if !content.is_empty() {
                            tc.content.extend(content.iter().cloned());
                        }
                        if !locations.is_empty() {
                            tc.locations = locations.clone();
                        }
                    }
                }
            }
            EventPayload::PermissionRequested { request_id, title, options, .. } => {
                if let Some(ti) = self.turns.len().checked_sub(1) {
                    let turn = &mut self.turns[ti];
                    turn.items.push(TurnItem::Permission {
                        request_id: request_id.clone(),
                        title: title.clone(),
                        options: options.clone(),
                        resolved_with: None,
                        auto: false,
                    });
                    self.perm_index.insert(request_id.clone(), (ti, turn.items.len() - 1));
                }
            }
            EventPayload::PermissionResolved { request_id, option_id, auto: is_auto } => {
                if let Some(&(t, i)) = self.perm_index.get(request_id) {
                    if let Some(TurnItem::Permission { resolved_with, auto, .. }) =
                        self.turns.get_mut(t).and_then(|tv| tv.items.get_mut(i))
                    {
                        *resolved_with = option_id.clone();
                        *auto = *is_auto;
                    }
                }
            }
            EventPayload::ConfigOptionsChanged { options } => {
                self.config_options = options.clone();
            }
            EventPayload::UsageChanged { used, size, cost } => {
                self.latest_usage = Some((*used, *size, cost.clone()));
            }
            EventPayload::PlanChanged { entries } => {
                self.plan = entries.clone();
            }
            EventPayload::UnknownUpdate { discriminant, raw } => {
                self.unknown_updates.push((discriminant.clone(), raw.clone()));
            }
            EventPayload::AgentError { message } => {
                if let Some(t) = self.turns.last_mut() {
                    t.items.push(TurnItem::Error { message: message.clone() });
                }
            }
            EventPayload::Run { run } => self.run_events.push(run.clone()),
            EventPayload::FileAccess {
                op,
                requested,
                resolved,
                allowed,
                refusal,
                bytes,
            } => {
                self.file_accesses.push(FileAccessRecord {
                    op: *op,
                    requested: requested.clone(),
                    resolved: resolved.clone(),
                    allowed: *allowed,
                    refusal: refusal.clone(),
                    bytes: *bytes,
                });
                if let Some(t) = self.turns.last_mut() {
                    t.items.push(TurnItem::File {
                        op: *op,
                        requested: requested.clone(),
                        resolved: resolved.clone(),
                        allowed: *allowed,
                        refusal: refusal.clone(),
                        bytes: *bytes,
                    });
                }
            }
            EventPayload::AgentExited { code, signal } => {
                if let Some(t) = self.turns.last_mut() {
                    t.items.push(TurnItem::Error {
                        message: format!("agent exited (code={code:?} signal={signal:?})"),
                    });
                    if t.stop_reason.is_none() {
                        t.stop_reason = Some(StopReason::Unknown);
                    }
                    for item in t.items.iter_mut() {
                        if let TurnItem::Segment(s) = item {
                            s.state = SegmentState::Settled;
                        }
                    }
                }
            }
        }
    }

    /// The percentage the context ring should show, or `None` when the agent never
    /// reported usage. `None` means *draw nothing* — not zero, not "unknown". ACP's own
    /// usage RFD refuses to define an absent state, so inventing one here would be us
    /// making up a number.
    pub fn context_percent(&self) -> Option<f64> {
        match self.latest_usage {
            Some((_, 0, _)) | None => None,
            Some((used, size, _)) => Some((used as f64 / size as f64) * 100.0),
        }
    }
}

#[cfg(test)]
mod file_access_view_tests {
    use super::*;
    use crate::event::FileOp;

    fn access(requested: &str, allowed: bool) -> EventPayload {
        EventPayload::FileAccess {
            op: FileOp::Write,
            requested: requested.to_string(),
            resolved: allowed.then(|| requested.to_string()),
            allowed,
            refusal: (!allowed).then(|| "outside-root".to_string()),
            bytes: allowed.then_some(12),
        }
    }

    fn built(payloads: &[EventPayload]) -> TurnView {
        let mut b = ViewBuilder::new();
        b.apply_all(payloads);
        b.into_turns().pop().expect("a turn")
    }

    /// The defect this guards. An agent that edits through the protocol's file methods — the path
    /// the daemon encourages, since it is the only one that is bounded and logged — produced a
    /// transcript containing a thought and an answer and no sign that any file had changed, because
    /// allowed accesses went only to the audit list.
    #[test]
    fn an_allowed_write_appears_in_the_turn() {
        let view = built(&[
            EventPayload::TurnStarted { turn: 1, prompt: "go".into() },
            access("/w/a.rs", true),
        ]);
        assert!(
            view.items.iter().any(|i| matches!(
                i,
                TurnItem::File { requested, allowed: true, .. } if requested == "/w/a.rs"
            )),
            "an allowed write must be in the transcript, not only in the audit list: {:?}",
            view.items
        );
    }

    /// Enforcement the reader cannot see is enforcement they cannot audit.
    #[test]
    fn a_refusal_appears_in_the_turn_with_its_reason() {
        let view = built(&[
            EventPayload::TurnStarted { turn: 1, prompt: "go".into() },
            access("/etc/passwd", false),
        ]);
        let found = view.items.iter().find_map(|i| match i {
            TurnItem::File { requested, allowed: false, refusal, .. } => {
                Some((requested.clone(), refusal.clone()))
            }
            _ => None,
        });
        assert_eq!(
            found,
            Some(("/etc/passwd".to_string(), Some("outside-root".to_string())))
        );
    }

    /// Order is the point of putting them in the turn at all: a read before a command ran and one
    /// after it tell different stories.
    #[test]
    fn accesses_keep_their_place_relative_to_everything_else() {
        let view = built(&[
            EventPayload::TurnStarted { turn: 1, prompt: "go".into() },
            access("/w/first.rs", true),
            EventPayload::ToolCallStarted {
                tool_call_id: "t1".into(),
                title: "Run tests".into(),
                kind: ToolKind::Execute,
                status: ToolStatus::InProgress,
            },
            access("/w/second.rs", true),
        ]);
        let order: Vec<&str> = view
            .items
            .iter()
            .filter_map(|i| match i {
                TurnItem::File { requested, .. } => Some(requested.as_str()),
                TurnItem::ToolCall(_) => Some("tool"),
                _ => None,
            })
            .collect();
        assert_eq!(order, vec!["/w/first.rs", "tool", "/w/second.rs"]);
    }
}
