//! Tests for turn segmentation.
//!
//! The scenarios here are the ones a naive fake agent cannot produce. A fake that emits
//! one thought, one tool call and one answer per turn will pass almost any segmentation
//! implementation, including a broken one. So every scenario below deliberately does at
//! least one of: multiple thoughts per turn, chunks with no `messageId`, a thought resumed
//! after a tool call, or a cancellation mid-tool-call.

use crate::event::*;
use crate::normalize::*;
use crate::turn::*;

fn thought(id: Option<&str>, text: &str) -> RawUpdate {
    RawUpdate::TextChunk {
        kind: SegmentKind::Thought,
        message_id: id.map(|s| s.to_string()),
        text: text.to_string(),
    }
}

fn answer(id: Option<&str>, text: &str) -> RawUpdate {
    RawUpdate::TextChunk {
        kind: SegmentKind::Message,
        message_id: id.map(|s| s.to_string()),
        text: text.to_string(),
    }
}

fn tool(id: &str, status: ToolStatus) -> RawUpdate {
    RawUpdate::ToolCall {
        tool_call_id: id.to_string(),
        title: format!("tool {id}"),
        kind: ToolKind::Read,
        status,
        content: Vec::new(),
        locations: Vec::new(),
    }
}

/// Drives a whole turn and returns the flat payload stream.
fn run(updates: Vec<RawUpdate>, stop: StopReason) -> Vec<EventPayload> {
    let mut n = Normalizer::new();
    let mut out = n.begin_turn("do the thing");
    for u in updates {
        out.extend(n.push(u));
    }
    out.extend(n.end_turn(stop));
    out
}

fn view(payloads: &[EventPayload]) -> Vec<TurnView> {
    let mut b = ViewBuilder::new();
    b.apply_all(payloads);
    b.into_turns()
}

#[test]
fn multiple_thoughts_in_one_turn_never_expand_together() {
    // The realistic shape: think, call a tool, think again, call another tool, think a
    // third time, then answer. Six segments' worth of interleaving in a single turn.
    let payloads = run(
        vec![
            thought(Some("m1"), "first I should look at the config"),
            tool("t1", ToolStatus::Completed),
            thought(Some("m2"), "that was not what I expected, check the loader"),
            tool("t2", ToolStatus::Completed),
            thought(Some("m3"), "now I understand the ordering"),
            answer(Some("m4"), "The loader reads the file twice."),
        ],
        StopReason::EndTurn,
    );

    assert_eq!(
        max_concurrent_live(&payloads),
        1,
        "at most one segment may be live at a time; more than one means the UI would \
         auto-expand several thought blocks and push the answer off screen"
    );

    let turns = view(&payloads);
    assert_eq!(turns.len(), 1);
    let t = &turns[0];
    assert!(t.live_segment().is_none(), "turn ended, nothing may remain live");
    assert!(
        t.items.iter().all(
            |i| !matches!(i, TurnItem::Segment(s) if s.state == SegmentState::Live)
        ),
        "no segment may be left live after the turn ends"
    );

    let thoughts: Vec<_> = t
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::Segment(s) if s.kind == SegmentKind::Thought => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(thoughts.len(), 3, "three distinct thought segments, not one merged blob");
    assert_eq!(thoughts[0].text, "first I should look at the config");
    assert_eq!(thoughts[2].text, "now I understand the ordering");
}

#[test]
fn only_the_newest_segment_is_live_mid_turn() {
    let mut n = Normalizer::new();
    let mut acc = n.begin_turn("go");
    acc.extend(n.push(thought(Some("m1"), "a")));
    acc.extend(n.push(tool("t1", ToolStatus::InProgress)));
    acc.extend(n.push(thought(Some("m2"), "b")));

    // Mid-turn snapshot: m1 settled, m2 live.
    let turns = view(&acc);
    let t = &turns[0];
    let live = t.live_segment().expect("m2 should be live mid-turn");
    assert_eq!(live.id.raw, "m2");
    assert_eq!(live.text, "b");

    let m1 = t
        .items
        .iter()
        .find_map(|i| match i {
            TurnItem::Segment(s) if s.id.raw == "m1" => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(m1.state, SegmentState::Settled, "an earlier thought must not stay live");
}

#[test]
fn chunks_without_message_id_split_on_tool_calls() {
    // v1 agents may omit `messageId` entirely. Without a fallback boundary every chunk in
    // the turn would collapse into one segment and the "thought / execute / answer"
    // layering would degrade to a single grey wall.
    let payloads = run(
        vec![
            thought(None, "looking"),
            thought(None, " deeper"),
            tool("t1", ToolStatus::Completed),
            thought(None, "second thought"),
            answer(None, "done"),
        ],
        StopReason::EndTurn,
    );

    assert_eq!(max_concurrent_live(&payloads), 1);

    let turns = view(&payloads);
    let t = &turns[0];
    assert!(
        t.segmentation_best_effort,
        "a turn segmented without agent-supplied ids must be marked best-effort so the \
         UI can avoid implying the agent drew these boundaries"
    );

    let segs: Vec<_> = t
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::Segment(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(segs.len(), 3, "two thoughts split by the tool call, plus the answer");
    assert_eq!(segs[0].text, "looking deeper", "consecutive chunks of one kind coalesce");
    assert_eq!(segs[1].text, "second thought");
    assert_eq!(segs[2].kind, SegmentKind::Message);
}

#[test]
fn switching_kind_without_message_id_also_splits() {
    let payloads = run(
        vec![thought(None, "thinking"), answer(None, "answering")],
        StopReason::EndTurn,
    );
    let turns = view(&payloads);
    let segs: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::Segment(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(segs.len(), 2, "thought and answer must never merge into one segment");
    assert_eq!(segs[0].kind, SegmentKind::Thought);
    assert_eq!(segs[1].kind, SegmentKind::Message);
}

#[test]
fn resuming_a_message_id_after_a_tool_call_reopens_that_segment() {
    // Upsert semantics: the agent returns to m1 after the tool call. The text belongs to
    // the original segment, and we must not create a duplicate.
    let payloads = run(
        vec![
            thought(Some("m1"), "start"),
            tool("t1", ToolStatus::Completed),
            thought(Some("m1"), " and continue"),
        ],
        StopReason::EndTurn,
    );

    assert_eq!(max_concurrent_live(&payloads), 1);

    let turns = view(&payloads);
    let segs: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::Segment(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(segs.len(), 1, "same messageId must not produce a second segment");
    assert_eq!(segs[0].text, "start and continue");
}

#[test]
fn cancelling_a_turn_settles_open_tool_calls() {
    // ACP: on cancellation the client SHOULD surface unfinished tool calls as cancelled.
    // Leaving them `in_progress` is what produces cards that spin forever.
    let payloads = run(
        vec![thought(Some("m1"), "thinking"), tool("t1", ToolStatus::InProgress)],
        StopReason::Cancelled,
    );

    let turns = view(&payloads);
    let tc = turns[0]
        .items
        .iter()
        .find_map(|i| match i {
            TurnItem::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .unwrap();
    assert_eq!(tc.status, ToolStatus::Cancelled);
    assert_eq!(turns[0].stop_reason, Some(StopReason::Cancelled));
    assert!(turns[0].live_segment().is_none());
}

#[test]
fn a_second_begin_turn_does_not_leak_a_live_segment() {
    // Guards against the shape where an agent errors out and the caller starts a new turn
    // without an explicit end. The previous turn's spinner must not run forever.
    let mut n = Normalizer::new();
    let mut acc = n.begin_turn("one");
    acc.extend(n.push(thought(Some("m1"), "half a thought")));
    acc.extend(n.begin_turn("two"));

    assert_eq!(max_concurrent_live(&acc), 1);
    let turns = view(&acc);
    assert_eq!(turns.len(), 2);
    assert!(turns[0].live_segment().is_none(), "abandoned turn must be settled");
}

#[test]
fn tool_call_update_for_an_unseen_call_creates_it() {
    // v2 makes the first update create the call, and v1 agents in the wild sometimes send
    // an update we have no `tool_call` for. Dropping it would silently lose a diff.
    let mut n = Normalizer::new();
    let mut acc = n.begin_turn("go");
    acc.extend(n.push(RawUpdate::ToolCallUpdate {
        tool_call_id: "orphan".into(),
        title: Some("edit main.rs".into()),
        status: Some(ToolStatus::Completed),
        content: vec![ToolContent::Diff {
            path: "/repo/main.rs".into(),
            old_text: Some("a".into()),
            new_text: "b".into(),
        }],
        locations: vec![ToolLocation { path: "/repo/main.rs".into(), line: Some(3) }],
    }));
    acc.extend(n.end_turn(StopReason::EndTurn));

    let turns = view(&acc);
    let tc = turns[0]
        .items
        .iter()
        .find_map(|i| match i {
            TurnItem::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("an orphan update must still produce a card");
    assert_eq!(tc.title, "edit main.rs");
    assert_eq!(tc.status, ToolStatus::Completed);
    assert!(matches!(tc.content[0], ToolContent::Diff { .. }));
    assert_eq!(tc.locations[0].line, Some(3));
}

#[test]
fn unknown_session_update_is_recorded_not_dropped() {
    // ACP documents that the variant set is not exhaustive. An unmodelled variant has to
    // land somewhere visible, or a future protocol change looks like silence.
    let mut n = Normalizer::new();
    let mut acc = n.begin_turn("go");
    acc.extend(n.push(RawUpdate::Unknown {
        discriminant: "future_thing".into(),
        raw: r#"{"sessionUpdate":"future_thing","x":1}"#.into(),
    }));
    acc.extend(n.end_turn(StopReason::EndTurn));

    let mut b = ViewBuilder::new();
    b.apply_all(&acc);
    assert_eq!(b.unknown_updates.len(), 1);
    assert_eq!(b.unknown_updates[0].0, "future_thing");
}

#[test]
fn context_percent_is_absent_until_the_agent_reports_usage() {
    let mut b = ViewBuilder::new();
    b.apply_all(&run(vec![answer(Some("m1"), "hi")], StopReason::EndTurn));
    assert_eq!(
        b.context_percent(),
        None,
        "with no usage_update the ring must be absent entirely, not zero and not 'unknown'"
    );

    b.apply(&EventPayload::UsageChanged { used: 53_000, size: 200_000, cost: None });
    let pct = b.context_percent().expect("once reported, show it");
    assert!((pct - 26.5).abs() < 1e-9);
}

#[test]
fn context_percent_survives_a_zero_window_without_dividing_by_zero() {
    let mut b = ViewBuilder::new();
    b.apply(&EventPayload::UsageChanged { used: 10, size: 0, cost: None });
    assert_eq!(b.context_percent(), None);
}

#[test]
fn config_options_are_passed_through_verbatim_including_unknown_categories() {
    // The UI must not hardcode model names, and an unrecognized category must degrade to
    // a plain select rather than being dropped.
    let mut b = ViewBuilder::new();
    b.apply(&EventPayload::ConfigOptionsChanged {
        options: vec![
            ConfigOptionView {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: Some("model".into()),
                value: ConfigValueView::Select {
                    current: "model-1".into(),
                    options: vec![ConfigChoice {
                        value: "model-1".into(),
                        name: "Model 1".into(),
                        description: Some("The fastest model".into()),
                    }],
                },
                live_switchable: true,
            },
            ConfigOptionView {
                id: "x".into(),
                name: "Experimental".into(),
                description: None,
                category: Some("_vendor_thing".into()),
                value: ConfigValueView::Boolean { current: false },
                live_switchable: false,
            },
        ],
    });
    assert_eq!(b.config_options.len(), 2);
    assert_eq!(b.config_options[1].category.as_deref(), Some("_vendor_thing"));
    assert!(!b.config_options[1].live_switchable);
}

/// Property-style check: the live-segment invariant must hold for every prefix of a
/// pseudo-random interleaving, not just the curated scenarios above.
#[test]
fn live_invariant_holds_under_randomized_interleaving() {
    // Deterministic LCG so a failure is reproducible without a dependency.
    let mut state: u64 = 0x2545F491_4F6CDD1D;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };

    for case in 0..200 {
        let mut n = Normalizer::new();
        let mut acc = n.begin_turn("fuzz");
        let len = 3 + (next() % 25) as usize;
        for i in 0..len {
            let pick = next() % 6;
            let with_id = next() % 2 == 0;
            let id = format!("m{}", next() % 4);
            let u = match pick {
                0 | 1 => thought(if with_id { Some(&id) } else { None }, "t"),
                2 => answer(if with_id { Some(&id) } else { None }, "a"),
                3 => tool(&format!("tc{i}"), ToolStatus::InProgress),
                4 => RawUpdate::ToolCallUpdate {
                    tool_call_id: format!("tc{i}"),
                    title: None,
                    status: Some(ToolStatus::Completed),
                    content: Vec::new(),
                    locations: Vec::new(),
                },
                _ => RawUpdate::Usage { used: 1, size: 100, cost: None },
            };
            acc.extend(n.push(u));

            assert!(
                max_concurrent_live(&acc) <= 1,
                "case {case}: invariant broken after {i} updates"
            );
        }
        acc.extend(n.end_turn(StopReason::EndTurn));
        // At most one, not exactly one: a turn made purely of tool calls and usage
        // updates legitimately contains no text segment at all.
        assert!(max_concurrent_live(&acc) <= 1, "case {case}: invariant broken at end of turn");
        let turns = view(&acc);
        assert!(
            turns.last().unwrap().live_segment().is_none(),
            "case {case}: a completed turn must have no live segment"
        );
    }
}
