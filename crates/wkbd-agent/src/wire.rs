//! Mapping ACP wire messages onto the internal event model.
//!
//! Parsing here is deliberately liberal. ACP documents that its `sessionUpdate` variant
//! set is not exhaustive, agents ship enum values ahead of the spec, and at least one
//! widely used agent has shipped a build that prints a plain-text banner into the
//! JSON-RPC stream. None of those may take down a session: an update we cannot model
//! becomes `RawUpdate::Unknown`, an unparseable line becomes a logged warning, and the
//! session carries on.
//!
//! Outgoing messages are the opposite: they are built from typed structs, and a test
//! asserts they deserialize into the official protocol crate's types, so we cannot
//! silently drift out of spec in the direction we control.

use serde_json::Value;
use wkbd_proto::{
    ConfigChoice, ConfigOptionView, ConfigValueView, CostView, PermissionOption,
    PermissionOptionKind, PlanEntryView, PromptCapabilities, RawUpdate, SegmentKind, StopReason,
    ToolContent,
    ToolKind, ToolLocation, ToolStatus,
};

pub fn parse_tool_kind(s: Option<&str>) -> ToolKind {
    match s {
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("delete") => ToolKind::Delete,
        Some("move") => ToolKind::Move,
        Some("search") => ToolKind::Search,
        Some("execute") => ToolKind::Execute,
        Some("think") => ToolKind::Think,
        Some("fetch") => ToolKind::Fetch,
        Some("switch_mode") => ToolKind::SwitchMode,
        Some("other") | None => ToolKind::Other,
        Some(other) => ToolKind::Unknown(other.to_string()),
    }
}

/// Unrecognized statuses map to `InProgress` rather than being dropped.
///
/// The alternative is a card stuck at "pending" forever. Choosing in-progress means a
/// future terminal status we do not know about still leaves a spinner, but the turn's
/// own end settles it, so nothing spins indefinitely.
pub fn parse_tool_status(s: Option<&str>) -> Option<ToolStatus> {
    match s {
        Some("pending") => Some(ToolStatus::Pending),
        Some("in_progress") => Some(ToolStatus::InProgress),
        Some("completed") => Some(ToolStatus::Completed),
        Some("failed") => Some(ToolStatus::Failed),
        Some("cancelled") => Some(ToolStatus::Cancelled),
        Some(_) => Some(ToolStatus::InProgress),
        None => None,
    }
}

pub fn parse_stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("max_turn_requests") => StopReason::MaxTurnRequests,
        Some("refusal") => StopReason::Refusal,
        Some("cancelled") => StopReason::Cancelled,
        _ => StopReason::Unknown,
    }
}

fn text_of_content(content: &Value) -> Option<String> {
    // The common case is `{"type":"text","text":"..."}`. Anything else is summarized
    // rather than dropped, so a future content kind still shows the user that something
    // arrived instead of a silent gap in the transcript.
    match content.get("type").and_then(|t| t.as_str()) {
        Some("text") => content.get("text").and_then(|t| t.as_str()).map(str::to_string),
        Some(other) => Some(format!("[{other} content]")),
        None => content.as_str().map(str::to_string),
    }
}

fn parse_tool_content(items: Option<&Value>) -> Vec<ToolContent> {
    let Some(arr) = items.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
            Some("diff") => Some(ToolContent::Diff {
                path: item.get("path")?.as_str()?.to_string(),
                old_text: item.get("oldText").and_then(|v| v.as_str()).map(str::to_string),
                new_text: item.get("newText").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            }),
            Some("terminal") => Some(ToolContent::Terminal {
                terminal_id: item.get("terminalId")?.as_str()?.to_string(),
            }),
            Some("content") => item
                .get("content")
                .and_then(text_of_content)
                .map(|text| ToolContent::Text { text }),
            _ => item.get("text").and_then(|t| t.as_str()).map(|t| ToolContent::Text {
                text: t.to_string(),
            }),
        })
        .collect()
}

fn parse_locations(items: Option<&Value>) -> Vec<ToolLocation> {
    let Some(arr) = items.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            Some(ToolLocation {
                path: item.get("path")?.as_str()?.to_string(),
                line: item.get("line").and_then(|v| v.as_u64()).map(|v| v as u32),
            })
        })
        .collect()
}

pub fn parse_permission_options(items: Option<&Value>) -> Vec<PermissionOption> {
    let Some(arr) = items.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let option_id = item.get("optionId")?.as_str()?.to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&option_id)
                .to_string();
            let kind = match item.get("kind").and_then(|v| v.as_str()) {
                Some("allow_once") => PermissionOptionKind::AllowOnce,
                Some("allow_always") => PermissionOptionKind::AllowAlways,
                Some("reject_once") => PermissionOptionKind::RejectOnce,
                Some("reject_always") => PermissionOptionKind::RejectAlways,
                _ => PermissionOptionKind::Unknown,
            };
            Some(PermissionOption { option_id, name, kind })
        })
        .collect()
}

/// Reads `agentCapabilities.promptCapabilities` out of the `initialize` response.
///
/// Everything absent means unsupported, which the specification is explicit about: a capability
/// omitted from the handshake **MUST** be treated as unsupported, precisely so that adding one
/// later is not a breaking change. So this is one of the few places where a missing field must
/// not be read generously — a wrong `true` here puts a content block on the wire that the agent
/// never agreed to accept.
///
/// Note what is *not* here. Text and resource links are the baseline every agent must accept, so
/// there is no capability to read and no control to hide.
pub fn parse_prompt_capabilities(init_result: &Value) -> PromptCapabilities {
    let caps = init_result.get("agentCapabilities").and_then(|c| c.get("promptCapabilities"));
    let flag = |name: &str| {
        caps.and_then(|c| c.get(name)).and_then(|v| v.as_bool()).unwrap_or(false)
    };
    PromptCapabilities {
        image: flag("image"),
        audio: flag("audio"),
        embedded_context: flag("embeddedContext"),
    }
}

/// Parses the `configOptions` array from `session/new` or a `config_option_update`.
///
/// `live_switchable` cannot be read off the wire: the protocol has no field for "can this
/// be changed after the process started". It is supplied by the caller from what the
/// capability probe observed, because the honest answer differs per agent and guessing
/// produces a dropdown that looks functional and changes nothing.
pub fn parse_config_options(items: Option<&Value>, live_switchable: bool) -> Vec<ConfigOptionView> {
    let Some(arr) = items.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
            let description =
                item.get("description").and_then(|v| v.as_str()).map(str::to_string);
            // Kept as a free string rather than an enum. The spec reserves unprefixed
            // names and leaves `_`-prefixed ones to vendors, and requires clients to
            // handle unknown categories gracefully; turning it into an enum here would
            // mean an unknown category either panics or silently becomes "other".
            let category = item.get("category").and_then(|v| v.as_str()).map(str::to_string);

            let value = match item.get("type").and_then(|v| v.as_str()) {
                Some("boolean") => ConfigValueView::Boolean {
                    current: item.get("currentValue").and_then(|v| v.as_bool()).unwrap_or(false),
                },
                // Absent `type` defaults to select, matching the wire format where the
                // value-id variant is the default.
                _ => ConfigValueView::Select {
                    current: item
                        .get("currentValue")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    options: item
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|opts| {
                            opts.iter()
                                .filter_map(|o| {
                                    let value = o.get("value")?.as_str()?.to_string();
                                    Some(ConfigChoice {
                                        name: o
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or(&value)
                                            .to_string(),
                                        description: o
                                            .get("description")
                                            .and_then(|v| v.as_str())
                                            .map(str::to_string),
                                        value,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            };

            Some(ConfigOptionView { id, name, description, category, value, live_switchable })
        })
        .collect()
}

/// Maps one `session/update` payload onto a `RawUpdate`.
///
/// Returns `Unknown` rather than an error for anything unmodelled. The raw JSON travels
/// with it so the message inspector can show exactly what was ignored.
pub fn map_session_update(update: &Value, live_switchable: bool) -> RawUpdate {
    let disc = update.get("sessionUpdate").and_then(|v| v.as_str()).unwrap_or("");
    let message_id = update.get("messageId").and_then(|v| v.as_str()).map(str::to_string);

    match disc {
        "agent_thought_chunk" | "agent_message_chunk" | "user_message_chunk" => {
            let kind = match disc {
                "agent_thought_chunk" => SegmentKind::Thought,
                "user_message_chunk" => SegmentKind::UserEcho,
                _ => SegmentKind::Message,
            };
            match update.get("content").and_then(text_of_content) {
                Some(text) => RawUpdate::TextChunk { kind, message_id, text },
                None => RawUpdate::Unknown {
                    discriminant: format!("{disc}(uninterpretable content)"),
                    raw: update.to_string(),
                },
            }
        }
        "tool_call" => {
            let Some(tool_call_id) =
                update.get("toolCallId").and_then(|v| v.as_str()).map(str::to_string)
            else {
                return RawUpdate::Unknown {
                    discriminant: "tool_call(no id)".into(),
                    raw: update.to_string(),
                };
            };
            RawUpdate::ToolCall {
                title: update
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tool_call_id)
                    .to_string(),
                kind: parse_tool_kind(update.get("kind").and_then(|v| v.as_str())),
                status: parse_tool_status(update.get("status").and_then(|v| v.as_str()))
                    .unwrap_or(ToolStatus::Pending),
                content: parse_tool_content(update.get("content")),
                locations: parse_locations(update.get("locations")),
                tool_call_id,
            }
        }
        "tool_call_update" => {
            let Some(tool_call_id) =
                update.get("toolCallId").and_then(|v| v.as_str()).map(str::to_string)
            else {
                return RawUpdate::Unknown {
                    discriminant: "tool_call_update(no id)".into(),
                    raw: update.to_string(),
                };
            };
            RawUpdate::ToolCallUpdate {
                tool_call_id,
                title: update.get("title").and_then(|v| v.as_str()).map(str::to_string),
                status: parse_tool_status(update.get("status").and_then(|v| v.as_str())),
                content: parse_tool_content(update.get("content")),
                locations: parse_locations(update.get("locations")),
            }
        }
        // Both plan shapes.
        //
        // v1 flattens `entries` onto the update. v2 removed that variant entirely and replaced it with
        // `plan_update`, which nests a `plan` object carrying `planId` and `entries`. Reading only the
        // v1 position — which is what this did — matches the v2 discriminator, produces an empty entry
        // list, and reports no unknown update: the plan arrives and is silently blank, which is the
        // worst of the three possible failures.
        //
        // The synthetic id for the v1 shape is `main`, which is what the spec tells adapters to use
        // when mapping v1 forward.
        "plan" | "plan_update" => {
            let nested = update.get("plan");
            let plan_id = nested
                .and_then(|p| p.get("planId").or_else(|| p.get("plan_id")))
                .and_then(|v| v.as_str())
                .unwrap_or("main")
                .to_string();
            let entries = nested
                .and_then(|p| p.get("entries"))
                .or_else(|| update.get("entries"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            Some(PlanEntryView {
                                content: e.get("content")?.as_str()?.to_string(),
                                // Kept verbatim, including values this build has never heard of. The
                                // spec reserves plain names for future variants and requires custom
                                // ones to start with `_`, so an unrecognised value is either a newer
                                // protocol or a deliberate extension — and mapping it onto `medium`
                                // or `pending` would state something the agent did not.
                                priority: e
                                    .get("priority")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("medium")
                                    .to_string(),
                                status: e
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("pending")
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            RawUpdate::Plan { plan_id, entries }
        }

        "config_option_update" => RawUpdate::ConfigOptions {
            options: parse_config_options(update.get("configOptions"), live_switchable),
        },
        "usage_update" => {
            // `used` and `size` are both required by the schema. If either is missing the
            // agent is not really reporting usage, and inventing a denominator would put
            // a number on screen that means nothing.
            match (
                update.get("used").and_then(|v| v.as_u64()),
                update.get("size").and_then(|v| v.as_u64()),
            ) {
                (Some(used), Some(size)) => RawUpdate::Usage {
                    used,
                    size,
                    cost: update.get("cost").and_then(|c| {
                        Some(CostView {
                            amount: c.get("amount")?.as_f64()?,
                            currency: c
                                .get("currency")
                                .and_then(|v| v.as_str())
                                .unwrap_or("USD")
                                .to_string(),
                        })
                    }),
                },
                _ => RawUpdate::Unknown {
                    discriminant: "usage_update(incomplete)".into(),
                    raw: update.to_string(),
                },
            }
        }
        other => RawUpdate::Unknown {
            discriminant: if other.is_empty() { "(missing)".into() } else { other.to_string() },
            raw: update.to_string(),
        },
    }
}

#[cfg(test)]
mod plan_shape_tests {
    use super::*;
    use serde_json::json;

    fn parse(update: serde_json::Value) -> RawUpdate {
        map_session_update(&update, false)
    }

    /// The v1 shape, which flattens the entries onto the update.
    #[test]
    fn reads_the_flat_v1_plan() {
        let got = parse(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Read the loader", "priority": "high", "status": "in_progress" },
                { "content": "Add a cache", "priority": "medium", "status": "pending" }
            ]
        }));
        match got {
            RawUpdate::Plan { plan_id, entries } => {
                // The synthetic id the spec tells adapters to use when mapping v1 forward, so that a
                // v1 plan and a v2 plan land in the same slot rather than accumulating as two.
                assert_eq!(plan_id, "main");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].status, "in_progress");
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    /// The v2 shape, which nests them under a `plan` object with an id. Reading only the v1 position
    /// matched this discriminator, produced an empty entry list, and reported no unknown update: the
    /// plan arrived silently blank, which is the worst of the three possible failures.
    #[test]
    fn reads_the_nested_v2_plan_update() {
        let got = parse(json!({
            "sessionUpdate": "plan_update",
            "plan": {
                "type": "items",
                "planId": "plan-1",
                "entries": [
                    { "content": "Check for syntax errors", "priority": "high", "status": "pending" }
                ]
            }
        }));
        match got {
            RawUpdate::Plan { plan_id, entries } => {
                assert_eq!(plan_id, "plan-1");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].content, "Check for syntax errors");
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    /// Two plans at once, which v2 allows and requires the client to keep apart.
    #[test]
    fn distinguishes_two_concurrent_plans() {
        let a = parse(json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "strategy", "entries": [] }
        }));
        let b = parse(json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "checklist", "entries": [] }
        }));
        match (a, b) {
            (RawUpdate::Plan { plan_id: x, .. }, RawUpdate::Plan { plan_id: y, .. }) => {
                assert_ne!(x, y);
            }
            other => panic!("expected two plans, got {other:?}"),
        }
    }

    /// The spec reserves plain names for future variants and requires custom ones to start with `_`,
    /// so an unrecognised value is either a newer protocol or a deliberate extension. Mapping it onto
    /// `medium` or `pending` would state something the agent did not.
    #[test]
    fn keeps_a_status_this_build_has_never_heard_of() {
        let got = parse(json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "main", "entries": [
                { "content": "Wait for review", "priority": "_urgent", "status": "_blocked" }
            ]}
        }));
        match got {
            RawUpdate::Plan { entries, .. } => {
                assert_eq!(entries[0].priority, "_urgent");
                assert_eq!(entries[0].status, "_blocked");
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    /// A missing field defaults rather than dropping the entry: an entry with no stated priority is
    /// still an entry, and losing it would understate the plan.
    #[test]
    fn an_entry_without_a_priority_still_counts() {
        let got = parse(json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "main", "entries": [{ "content": "Do it" }] }
        }));
        match got {
            RawUpdate::Plan { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].priority, "medium");
                assert_eq!(entries[0].status, "pending");
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }
}
