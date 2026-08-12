//! Capability probing.
//!
//! The protocol's own guidance is that wire compatibility must not be inferred from a
//! version number and that clients should decide what to use based on the capabilities
//! exchanged at `initialize`. That is necessary but not sufficient for a UI: several
//! things the interface depends on are not announced anywhere and can only be found out
//! by watching a real turn.
//!
//! - Whether content chunks carry `messageId`, which decides whether thought segmentation
//!   is the agent's own or our best guess.
//! - How many thought segments a turn actually produces. One is the case every naive test
//!   double produces; several is the case real agents produce and the one that breaks
//!   auto-expansion.
//! - Whether `usage_update` is ever sent, which decides whether the context ring exists.
//! - Whether `session/set_config_option` is honoured at runtime, which decides whether the
//!   model selector can switch in place or has to start a new session.
//!
//! Every field defaults to "not observed", and the UI treats not-observed as absent. That
//! makes the degraded path the default path, which is the right way round: a missing
//! control is honest, a control that does nothing is not.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::conn::{Connection, Incoming};
use crate::wire;
use wkbd_proto::{Normalizer, RawUpdate, SegmentKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSupport {
    /// The agent accepted `session/set_config_option` for this id.
    Live,
    /// The agent rejected it, so the setting can only be chosen at launch.
    LaunchOnly,
    /// Never tested.
    NotObserved,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityReport {
    pub agent_id: String,
    pub command: String,

    /// From `initialize`. `None` means the agent never answered.
    pub protocol_version: Option<i64>,
    pub agent_info: Option<String>,
    pub agent_capabilities: Option<Value>,
    pub auth_methods: Vec<String>,
    /// True when `initialize` failed or the process died first.
    pub initialize_failed: Option<String>,

    /// From `session/new`.
    pub session_created: bool,
    pub config_option_ids: Vec<String>,
    pub config_categories: Vec<String>,
    /// Per-option runtime settability, keyed by option id.
    pub config_support: std::collections::BTreeMap<String, ConfigSupport>,

    /// Observed during a real prompt turn. All of these are `false`/`0` when no turn was
    /// run, which the report distinguishes with `turn_observed`.
    pub turn_observed: bool,
    pub sent_message_ids: bool,
    pub omitted_message_ids: bool,
    pub thought_segments_in_turn: usize,
    pub message_segments_in_turn: usize,
    pub tool_calls_in_turn: usize,
    pub sent_usage_update: bool,
    pub sent_plan: bool,
    pub sent_diff_content: bool,
    pub sent_terminal_content: bool,
    pub requested_permission: bool,
    pub unknown_updates: Vec<String>,
    pub malformed_lines: usize,
    pub stop_reason: Option<String>,
    /// Must be at most 1. Anything higher against a real agent means the UI would expand
    /// several thought blocks at once, so the probe reports it rather than only the unit
    /// tests asserting it.
    pub max_concurrent_live_segments: usize,
}

impl CapabilityReport {
    /// Whether the context-usage indicator should exist for this agent at all.
    pub fn context_indicator_supported(&self) -> bool {
        self.sent_usage_update
    }

    /// Whether thought segmentation reflects the agent's own boundaries.
    pub fn segmentation_is_authoritative(&self) -> bool {
        self.sent_message_ids && !self.omitted_message_ids
    }

    pub fn summary_line(&self) -> String {
        format!(
            "{:<20} proto={:<4} cfg={:<2} live={:<2} msgid={:<11} thoughts={:<2} usage={:<5} diff={:<5} perm={:<5}",
            self.agent_id,
            self.protocol_version.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            self.config_option_ids.len(),
            self.config_support
                .values()
                .filter(|s| **s == ConfigSupport::Live)
                .count(),
            if !self.turn_observed {
                "not-observed"
            } else if self.segmentation_is_authoritative() {
                "yes"
            } else if self.sent_message_ids {
                "partial"
            } else {
                "no"
            },
            if self.turn_observed { self.thought_segments_in_turn.to_string() } else { "-".into() },
            if self.turn_observed { self.sent_usage_update.to_string() } else { "-".into() },
            if self.turn_observed { self.sent_diff_content.to_string() } else { "-".into() },
            if self.turn_observed { self.requested_permission.to_string() } else { "-".into() },
        )
    }
}

/// Probes one agent.
///
/// When `prompt` is `None` only the `initialize` and `session/new` half is measured. That
/// half needs no model credentials, which matters because it is the only half obtainable
/// on a machine without API keys — and reporting it while clearly marking the rest as
/// not-observed is more useful than reporting nothing.
pub async fn probe_agent(
    agent_id: &str,
    command_display: &str,
    build_command: impl FnOnce() -> tokio::process::Command,
    cwd: &str,
    prompt: Option<&str>,
    permission_answer: Option<&str>,
) -> Result<CapabilityReport> {
    let mut report = CapabilityReport {
        agent_id: agent_id.to_string(),
        command: command_display.to_string(),
        ..Default::default()
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<Incoming>();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    let conn = Arc::new(Connection::spawn(build_command(), tx, Some(raw_tx)).await?);

    match conn.request("initialize", json!({ "protocolVersion": 1 })).await {
        Ok(v) => {
            report.protocol_version = v.get("protocolVersion").and_then(|p| p.as_i64());
            report.agent_capabilities = v.get("agentCapabilities").cloned();
            report.agent_info = v
                .get("agentInfo")
                .and_then(|i| i.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string);
            report.auth_methods = v
                .get("authMethods")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }
        Err(e) => {
            report.initialize_failed = Some(e.to_string());
            let _ = conn.kill().await;
            return Ok(report);
        }
    }

    let session_id = match conn.request("session/new", json!({ "cwd": cwd, "mcpServers": [] })).await
    {
        Ok(v) => {
            report.session_created = true;
            let options = wire::parse_config_options(v.get("configOptions"), false);
            for o in &options {
                report.config_option_ids.push(o.id.clone());
                if let Some(c) = &o.category {
                    if !report.config_categories.contains(c) {
                        report.config_categories.push(c.clone());
                    }
                }
                report.config_support.insert(o.id.clone(), ConfigSupport::NotObserved);
            }
            v.get("sessionId").and_then(|s| s.as_str()).map(str::to_string)
        }
        Err(e) => {
            report.initialize_failed = Some(format!("session/new failed: {e}"));
            let _ = conn.kill().await;
            return Ok(report);
        }
    };

    let Some(session_id) = session_id else {
        report.initialize_failed = Some("session/new returned no sessionId".into());
        let _ = conn.kill().await;
        return Ok(report);
    };

    // Try setting each select option to its current value. Setting it to what it already
    // is means a success cannot change the agent's behaviour, so the probe stays safe to
    // run against a real agent; all we learn is whether the method is honoured.
    let existing = conn.request("session/new", json!({ "cwd": cwd, "mcpServers": [] })).await.ok();
    let probe_options = existing
        .as_ref()
        .and_then(|v| v.get("configOptions"))
        .cloned()
        .or_else(|| Some(json!([])));
    let opts = wire::parse_config_options(probe_options.as_ref(), false);
    for o in opts {
        let current = match &o.value {
            wkbd_proto::ConfigValueView::Select { current, .. } => json!(current),
            wkbd_proto::ConfigValueView::Boolean { current } => json!(current),
        };
        let accepted = conn
            .request(
                "session/set_config_option",
                json!({ "sessionId": session_id, "configId": o.id, "value": current }),
            )
            .await;
        report.config_support.insert(
            o.id.clone(),
            if accepted.is_ok() { ConfigSupport::Live } else { ConfigSupport::LaunchOnly },
        );
    }

    if let Some(prompt) = prompt {
        report.turn_observed = true;
        let mut normalizer = Normalizer::new();
        let mut payloads = normalizer.begin_turn(prompt);

        let prompt_conn = conn.clone();
        let sid = session_id.clone();
        let prompt_text = prompt.to_string();
        let prompt_task = tokio::spawn(async move {
            prompt_conn
                .request(
                    "session/prompt",
                    json!({
                        "sessionId": sid,
                        "prompt": [{ "type": "text", "text": prompt_text }]
                    }),
                )
                .await
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut turn_done = false;
        let mut prompt_task = prompt_task;

        while !turn_done {
            tokio::select! {
                res = &mut prompt_task => {
                    match res {
                        Ok(Ok(v)) => {
                            report.stop_reason = v
                                .get("stopReason")
                                .and_then(|s| s.as_str())
                                .map(str::to_string);
                        }
                        Ok(Err(e)) => report.stop_reason = Some(format!("error: {e}")),
                        Err(e) => report.stop_reason = Some(format!("join error: {e}")),
                    }
                    turn_done = true;
                }
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        Incoming::Notification { method, params } if method == "session/update" => {
                            let Some(update) = params.get("update") else { continue };
                            let raw = wire::map_session_update(update, false);
                            observe(&mut report, &raw);
                            payloads.extend(normalizer.push(raw));
                        }
                        Incoming::Notification { .. } => {}
                        Incoming::Request { id, method, params, .. } => {
                            if method == "session/request_permission" {
                                report.requested_permission = true;
                                let options = wire::parse_permission_options(params.get("options"));
                                let chosen = permission_answer
                                    .map(str::to_string)
                                    .or_else(|| options.first().map(|o| o.option_id.clone()));
                                match chosen {
                                    Some(option_id) => {
                                        let _ = conn.respond(&id, json!({
                                            "outcome": { "outcome": "selected", "optionId": option_id }
                                        })).await;
                                    }
                                    None => {
                                        let _ = conn.respond(&id, json!({
                                            "outcome": { "outcome": "cancelled" }
                                        })).await;
                                    }
                                }
                            } else {
                                // Anything else the agent asks for during a probe is
                                // refused rather than guessed at: fs/* and terminal/*
                                // would run real side effects on the host.
                                let _ = conn.respond_error(
                                    &id, -32601,
                                    "not supported during capability probing",
                                ).await;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    report.stop_reason = Some("probe timed out".into());
                    turn_done = true;
                }
            }
        }

        payloads.extend(normalizer.end_turn(wire::parse_stop_reason(report.stop_reason.as_deref())));

        // Segment identity only exists after normalization, so the counts that matter for
        // the UI — how many thought blocks one turn produced — are taken from here rather
        // than from the raw chunk stream.
        let (thoughts, messages) = count_segments(&payloads);
        report.thought_segments_in_turn = thoughts;
        report.message_segments_in_turn = messages;
        report.max_concurrent_live_segments = wkbd_proto::max_concurrent_live(&payloads);
    }

    while let Ok(frame) = raw_rx.try_recv() {
        if frame.malformed {
            report.malformed_lines += 1;
        }
    }

    let _ = conn.kill().await;
    Ok(report)
}

fn observe(report: &mut CapabilityReport, raw: &RawUpdate) {
    match raw {
        RawUpdate::TextChunk { message_id, .. } => match message_id {
            Some(_) => report.sent_message_ids = true,
            None => report.omitted_message_ids = true,
        },
        RawUpdate::ToolCall { content, .. } | RawUpdate::ToolCallUpdate { content, .. } => {
            report.tool_calls_in_turn += 1;
            for c in content {
                match c {
                    wkbd_proto::ToolContent::Diff { .. } => report.sent_diff_content = true,
                    wkbd_proto::ToolContent::Terminal { .. } => {
                        report.sent_terminal_content = true
                    }
                    _ => {}
                }
            }
        }
        RawUpdate::Plan { .. } => report.sent_plan = true,
        RawUpdate::Usage { .. } => report.sent_usage_update = true,
        RawUpdate::ConfigOptions { .. } => {}
        RawUpdate::Unknown { discriminant, .. } => {
            if !report.unknown_updates.contains(discriminant) {
                report.unknown_updates.push(discriminant.clone());
            }
        }
    }
}

/// Counts segments from a normalized payload stream, which is the only place segment
/// identity exists. Called by the probe binary after a turn.
pub fn count_segments(payloads: &[wkbd_proto::EventPayload]) -> (usize, usize) {
    let mut thoughts = 0;
    let mut messages = 0;
    for p in payloads {
        if let wkbd_proto::EventPayload::SegmentStarted { kind, .. } = p {
            match kind {
                SegmentKind::Thought => thoughts += 1,
                SegmentKind::Message => messages += 1,
                SegmentKind::UserEcho => {}
            }
        }
    }
    (thoughts, messages)
}
