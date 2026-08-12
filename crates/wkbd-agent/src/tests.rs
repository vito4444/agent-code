//! Seam tests.
//!
//! These drive a real child process over a real pipe. Unit-testing the mapper against
//! hand-written JSON is necessary but not sufficient: the failures that actually happen
//! live in the handover — what we send versus what the agent expects, what it sends versus
//! what we tolerate, and what happens when the process dies mid-request.

use super::*;
use crate::pool::{AgentSpec, LaunchConfig};
use crate::session::{Prelude, PreludeProvider, SessionOpenRequest, SessionPurpose};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use wkbd_proto::*;

/// Locates the test double. Walks up from the test binary rather than using
/// CARGO_BIN_EXE_*, which is only available for binaries in the same package.
fn fake_agent_path() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    // .../target/debug/deps/wkbd_agent-<hash>
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join("fake-acp-agent");
    assert!(
        candidate.exists(),
        "test double not built. run: cargo build -p fake-acp-agent\nlooked for {}",
        candidate.display()
    );
    candidate
}

fn spec_for(profile: &str, live_config: bool) -> AgentSpec {
    let mut env = BTreeMap::new();
    env.insert("FAKE_ACP_PROFILE".to_string(), profile.to_string());
    if live_config {
        env.insert("FAKE_ACP_LIVE_CONFIG".to_string(), "1".to_string());
    }
    AgentSpec {
        id: format!("fake-{profile}"),
        display_name: "Fake".into(),
        command: fake_agent_path().to_string_lossy().to_string(),
        args: vec![],
        env,
        live_config_ids: if live_config {
            vec!["model".to_string(), "thought_level".to_string()]
        } else {
            vec![]
        },
        launch_config: BTreeMap::new(),
    }
}

fn command_builder() -> impl FnMut(&AgentSpec, &LaunchConfig) -> tokio::process::Command {
    |spec: &AgentSpec, config: &LaunchConfig| {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args);
        // Constructed rather than inherited: an inherited environment is a configuration
        // channel we do not control.
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        for (k, v) in &config.values {
            if let Some(template) = spec.launch_config.get(k) {
                if let Some(rest) = template.strip_prefix("env:") {
                    if let Some((name, _)) = rest.split_once('=') {
                        cmd.env(name, v);
                    }
                }
            }
        }
        cmd.kill_on_drop(true);
        cmd
    }
}

struct FixedPrelude(Prelude);

impl PreludeProvider for FixedPrelude {
    fn prelude_for(&self, _project_root: &str, _purpose: SessionPurpose) -> Prelude {
        self.0.clone()
    }
}

/// Opens a session against the double and runs one turn, returning the normalized stream.
async fn run_turn(
    profile: &str,
    prompt: &str,
    live_config: bool,
) -> (Vec<EventPayload>, SessionHandle) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let handle = factory
        .open(
            SessionOpenRequest {
                spec: spec_for(profile, live_config),
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await
        .expect("open session");

    let mut payloads = handle.begin_turn(prompt).await;

    let conn = handle.connection();
    let sid = handle.acp_session_id.clone();
    let prompt_task = tokio::spawn(async move {
        conn.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }),
        )
        .await
    });

    let mut prompt_task = prompt_task;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let stop;

    loop {
        tokio::select! {
            res = &mut prompt_task => {
                stop = match res {
                    Ok(Ok(v)) => wire::parse_stop_reason(
                        v.get("stopReason").and_then(|s| s.as_str())
                    ),
                    _ => StopReason::Unknown,
                };
                break;
            }
            msg = rx.recv() => {
                let Some((_, msg)) = msg else { stop = StopReason::Unknown; break };
                payloads.extend(handle_incoming(&handle, msg).await);
            }
            _ = tokio::time::sleep_until(deadline) => {
                stop = StopReason::Unknown;
                break;
            }
        }
    }

    // The prompt response can win the race against notifications that were sent before it.
    // Ending the turn without draining what is already buffered loses the tail of the
    // transcript — typically the final answer and the last tool result. This drain is not
    // an optimization; without it the ordering between "the turn ended" and "here is what
    // happened during it" is decided by scheduler luck.
    while let Ok((_, msg)) = rx.try_recv() {
        payloads.extend(handle_incoming(&handle, msg).await);
    }

    payloads.extend(handle.end_turn(stop).await);
    (payloads, handle)
}

/// Processes one inbound message, including answering permission requests, and returns the
/// events it produced.
async fn handle_incoming(handle: &SessionHandle, msg: Incoming) -> Vec<EventPayload> {
    match msg {
        Incoming::Notification { method, params } if method == "session/update" => {
            match params.get("update") {
                Some(update) => handle.ingest_update(update).await,
                None => Vec::new(),
            }
        }
        Incoming::Notification { .. } => Vec::new(),
        Incoming::Request { id, method, params, .. } => {
            if method != "session/request_permission" {
                let _ = handle
                    .connection()
                    .respond_error(&id, -32601, "not supported by this client")
                    .await;
                return Vec::new();
            }
            let request_id = format!("{id}");
            let (mut out, options) = handle.note_permission_request(&request_id, &params).await;
            let chosen = options.first().map(|o| o.option_id.clone());
            match &chosen {
                Some(option_id) => {
                    handle
                        .connection()
                        .respond(
                            &id,
                            json!({
                                "outcome": { "outcome": "selected", "optionId": option_id }
                            }),
                        )
                        .await
                        .unwrap();
                }
                None => {
                    handle
                        .connection()
                        .respond(&id, json!({ "outcome": { "outcome": "cancelled" } }))
                        .await
                        .unwrap();
                }
            }
            out.extend(handle.note_permission_resolved(&request_id, chosen, false).await);
            out
        }
    }
}

#[tokio::test]
async fn a_real_turn_from_a_real_process_produces_several_thought_segments() {
    let (payloads, _h) = run_turn("rich", "why is config slow", false).await;

    assert_eq!(
        max_concurrent_live(&payloads),
        1,
        "against a real process, at most one segment may ever be live at once"
    );

    let mut b = ViewBuilder::new();
    b.apply_all(&payloads);
    let turns = b.turns();
    assert_eq!(turns.len(), 1);

    let thoughts: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::Segment(s) if s.kind == SegmentKind::Thought => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(
        thoughts.len(),
        3,
        "the rich profile thinks three separate times in one turn; collapsing them into \
         one segment or leaving several live is the bug this exists to catch"
    );
    assert!(thoughts.iter().all(|t| t.state == SegmentState::Settled));
    assert!(turns[0].live_segment().is_none());
    assert_eq!(turns[0].stop_reason, Some(StopReason::EndTurn));

    // The diff has to survive as a diff, not as grey text.
    let diffs: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|i| match i {
            TurnItem::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .flat_map(|tc| tc.content.iter())
        .filter(|c| matches!(c, ToolContent::Diff { .. }))
        .collect();
    assert_eq!(diffs.len(), 1, "the edit tool call must carry structured diff content");

    // And the permission request must appear as its own item.
    assert!(
        turns[0].items.iter().any(|i| matches!(i, TurnItem::Permission { .. })),
        "a permission request must be rendered inline, not swallowed"
    );

    assert_eq!(b.context_percent(), Some(26.5), "usage was reported, so show it");
    assert_eq!(b.plan.len(), 2);
}

#[tokio::test]
async fn an_agent_that_sends_no_message_ids_still_gets_split_thoughts() {
    let (payloads, handle) = run_turn("spartan", "find todos", false).await;

    assert_eq!(max_concurrent_live(&payloads), 1);
    assert!(
        handle.segmentation_is_best_effort().await,
        "must be flagged, so the UI does not imply the agent drew these boundaries"
    );

    let mut b = ViewBuilder::new();
    b.apply_all(&payloads);
    let thoughts = b.turns()[0]
        .items
        .iter()
        .filter(|i| matches!(i, TurnItem::Segment(s) if s.kind == SegmentKind::Thought))
        .count();
    assert_eq!(thoughts, 2, "two thoughts separated by a tool call");
}

#[tokio::test]
async fn a_spartan_agent_offers_no_config_options_and_no_context_indicator() {
    let (payloads, handle) = run_turn("spartan", "hello", false).await;

    assert!(
        handle.config_options.is_empty(),
        "no options declared means the UI must draw no selector at all; an empty menu \
         tells the user there is a choice here and it is broken"
    );

    let mut b = ViewBuilder::new();
    b.apply_all(&payloads);
    assert_eq!(
        b.context_percent(),
        None,
        "no usage_update means the ring is absent, not zero and not 'unknown'"
    );
}

#[tokio::test]
async fn unknown_updates_and_unknown_enum_values_do_not_end_the_session() {
    let (payloads, _h) = run_turn("alien", "hello", false).await;

    let mut b = ViewBuilder::new();
    b.apply_all(&payloads);

    assert!(
        !b.unknown_updates.is_empty(),
        "an unmodelled sessionUpdate must be recorded so a future protocol change is \
         visible as data rather than as silence"
    );
    assert!(b
        .unknown_updates
        .iter()
        .any(|(d, _)| d == "quantum_entanglement_update"));

    // An unknown tool kind becomes ToolKind::Unknown, and an unknown status does not
    // leave the card stuck.
    let tool = b.turns()[0]
        .items
        .iter()
        .find_map(|i| match i {
            TurnItem::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("the alien tool call must still render");
    assert_eq!(tool.kind, ToolKind::Unknown("telepathy".into()));

    // The session survived to deliver the final answer.
    let answered = b.turns()[0].items.iter().any(|i| {
        matches!(i, TurnItem::Segment(s)
            if s.kind == SegmentKind::Message && s.text.contains("survived"))
    });
    assert!(answered, "the turn must complete despite the unmodelled updates");
}

#[tokio::test]
async fn a_non_json_banner_on_stdout_does_not_break_the_handshake() {
    // A shipped agent build printed "Loaded cached credentials." to stdout and corrupted
    // the JSON-RPC stream. Skipping unparseable lines is what keeps that survivable.
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let mut spec = spec_for("spartan", false);
    spec.env.insert("FAKE_ACP_POLLUTE_STDOUT".into(), "1".into());

    let handle = factory
        .open(
            SessionOpenRequest {
                spec,
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await;

    assert!(handle.is_ok(), "a junk line before the handshake must not be fatal");
}

#[tokio::test]
async fn a_process_that_dies_mid_request_fails_the_request_instead_of_hanging() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let handle = factory
        .open(
            SessionOpenRequest {
                spec: spec_for("crash", false),
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await
        .unwrap();

    // The crash profile exits before answering. Without failing outstanding requests when
    // stdout closes, this await would never return.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        handle.send_prompt("do something"),
    )
    .await;

    assert!(result.is_ok(), "the request must be failed, not left pending forever");
    assert!(result.unwrap().is_err());
}

#[tokio::test]
async fn cancelling_settles_the_open_tool_call() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let handle = factory
        .open(
            SessionOpenRequest {
                spec: spec_for("stall", false),
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await
        .unwrap();

    let mut payloads = handle.begin_turn("run the suite").await;
    let conn = handle.connection();
    let sid = handle.acp_session_id.clone();
    let prompt_task = tokio::spawn(async move {
        conn.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }),
        )
        .await
    });

    // Wait until the tool call is open, then cancel.
    let mut saw_tool = false;
    while !saw_tool {
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
            Ok(Some((_, Incoming::Notification { method, params }))) if method == "session/update" => {
                if let Some(update) = params.get("update") {
                    if update.get("sessionUpdate").and_then(|v| v.as_str()) == Some("tool_call") {
                        saw_tool = true;
                    }
                    payloads.extend(handle.ingest_update(update).await);
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(saw_tool, "the stall profile must open a tool call before stalling");

    handle.cancel().await.unwrap();
    let stop = match tokio::time::timeout(std::time::Duration::from_secs(20), prompt_task).await {
        Ok(Ok(Ok(v))) => wire::parse_stop_reason(v.get("stopReason").and_then(|s| s.as_str())),
        _ => StopReason::Cancelled,
    };
    assert_eq!(stop, StopReason::Cancelled);

    payloads.extend(handle.end_turn(stop).await);
    let mut b = ViewBuilder::new();
    b.apply_all(&payloads);
    let tool = b.turns()[0]
        .items
        .iter()
        .find_map(|i| match i {
            TurnItem::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        tool.status,
        ToolStatus::Cancelled,
        "an open tool call must be settled as cancelled, not left spinning"
    );
}

#[tokio::test]
async fn the_process_pool_separates_sessions_whose_launch_settings_differ() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));

    // This agent cannot change the model at runtime, so the model is part of the key.
    let spec = spec_for("spartan", false);

    let mut a_values = BTreeMap::new();
    a_values.insert("model".to_string(), "fast".to_string());
    let mut b_values = BTreeMap::new();
    b_values.insert("model".to_string(), "deep".to_string());

    let cfg_a = LaunchConfig { values: a_values, cwd: "/tmp".into() };
    let cfg_b = LaunchConfig { values: b_values, cwd: "/tmp".into() };

    assert_ne!(
        cfg_a.fingerprint(&spec),
        cfg_b.fingerprint(&spec),
        "different launch-only settings must not share a process; sharing is what makes \
         changing a model restart every session for that agent"
    );

    pool.acquire(&spec, &cfg_a, command_builder()).await.unwrap();
    pool.acquire(&spec, &cfg_b, command_builder()).await.unwrap();
    assert_eq!(pool.process_count().await, 2);

    // A second session with identical settings reuses the process.
    pool.acquire(&spec, &cfg_a, command_builder()).await.unwrap();
    assert_eq!(pool.process_count().await, 2);

    pool.shutdown().await;
}

#[tokio::test]
async fn the_process_pool_shares_a_process_when_the_agent_can_switch_at_runtime() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));

    // This agent honours session/set_config_option for `model`, so the model is excluded
    // from the fingerprint and two models can share one process.
    let spec = spec_for("rich", true);

    let mut a = BTreeMap::new();
    a.insert("model".to_string(), "fake-fast".to_string());
    let mut b = BTreeMap::new();
    b.insert("model".to_string(), "fake-deep".to_string());

    let cfg_a = LaunchConfig { values: a, cwd: "/tmp".into() };
    let cfg_b = LaunchConfig { values: b, cwd: "/tmp".into() };
    assert_eq!(cfg_a.fingerprint(&spec), cfg_b.fingerprint(&spec));

    pool.acquire(&spec, &cfg_a, command_builder()).await.unwrap();
    pool.acquire(&spec, &cfg_b, command_builder()).await.unwrap();
    assert_eq!(pool.process_count().await, 1);

    pool.shutdown().await;
}

#[tokio::test]
async fn a_runtime_config_change_is_reported_honestly_when_the_agent_refuses() {
    // The agent in this configuration only reads its model at startup. The selector must
    // learn that rather than appearing to work.
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let mut spec = spec_for("rich", false);
    // Claim liveness the agent does not have, so the refusal comes from the agent rather
    // than from our own bookkeeping.
    spec.live_config_ids = vec!["model".into()];

    let handle = factory
        .open(
            SessionOpenRequest {
                spec,
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await
        .unwrap();

    let out = handle.set_config_option("model", json!("fake-deep")).await.unwrap();
    assert!(
        out.is_none(),
        "a refusal must surface as None so the caller can say a restart is required"
    );
}

#[tokio::test]
async fn a_runtime_config_change_succeeds_when_the_agent_supports_it() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let pool = Arc::new(AgentPool::new(tx, None));
    let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(Prelude::default())));

    let handle = factory
        .open(
            SessionOpenRequest {
                spec: spec_for("rich", true),
                config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                project_root: "/tmp".into(),
                purpose: SessionPurpose::NewChat,
                resume_acp_session_id: None,
                handoff_summary: None,
            },
            command_builder(),
        )
        .await
        .unwrap();

    let options = handle
        .set_config_option("model", json!("fake-deep"))
        .await
        .unwrap()
        .expect("this agent honours runtime changes");
    let model = options.iter().find(|o| o.id == "model").unwrap();
    match &model.value {
        ConfigValueView::Select { current, .. } => assert_eq!(current, "fake-deep"),
        other => panic!("expected a select, got {other:?}"),
    }
}

#[tokio::test]
async fn config_options_survive_absent_and_unknown_categories() {
    let (_p, handle) = {
        let (tx, _rx) = mpsc::unbounded_channel();
        let pool = Arc::new(AgentPool::new(tx, None));
        let factory = SessionFactory::new(pool.clone(), Arc::new(FixedPrelude(Prelude::default())));
        let h = factory
            .open(
                SessionOpenRequest {
                    spec: spec_for("rich", true),
                    config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                    project_root: "/tmp".into(),
                    purpose: SessionPurpose::NewChat,
                    resume_acp_session_id: None,
                    handoff_summary: None,
                },
                command_builder(),
            )
            .await
            .unwrap();
        (pool, h)
    };

    let ids: Vec<&str> = handle.config_options.iter().map(|o| o.id.as_str()).collect();
    assert!(ids.contains(&"model"));
    assert!(ids.contains(&"thought_level"));
    // No category at all: still rendered.
    let verbose = handle.config_options.iter().find(|o| o.id == "verbose").unwrap();
    assert!(verbose.category.is_none());
    assert!(matches!(verbose.value, ConfigValueView::Boolean { current: false }));
    // Vendor-private category: kept as-is rather than coerced or dropped.
    let vendor = handle.config_options.iter().find(|o| o.id == "_vendor_mode").unwrap();
    assert_eq!(vendor.category.as_deref(), Some("_vendor_private"));
    // Liveness comes from the spec, not from the wire.
    assert!(handle.config_options.iter().find(|o| o.id == "model").unwrap().live_switchable);
    assert!(!vendor.live_switchable);
}

/// The entry-point audit. Every way a session can be created must apply the prelude.
#[tokio::test]
async fn every_session_purpose_receives_the_prelude() {
    let prelude = Prelude {
        rules: vec!["Always answer in Chinese".into()],
        memories: vec!["[confidence 0.8] this repo uses pnpm".into()],
    };

    for purpose in SessionPurpose::ALL {
        let (tx, _rx) = mpsc::unbounded_channel();
        let pool = Arc::new(AgentPool::new(tx, None));
        let factory = SessionFactory::new(pool, Arc::new(FixedPrelude(prelude.clone())));

        let handle = factory
            .open(
                SessionOpenRequest {
                    spec: spec_for("spartan", false),
                    config: LaunchConfig { values: BTreeMap::new(), cwd: "/tmp".into() },
                    project_root: "/tmp".into(),
                    purpose: *purpose,
                    // Resume is exercised with an id the double does not know, which also
                    // covers the fallback from a failed session/load to a new session.
                    resume_acp_session_id: matches!(purpose, SessionPurpose::Resume)
                        .then(|| "no-such-session".to_string()),
                    handoff_summary: matches!(purpose, SessionPurpose::RelaunchForConfig)
                        .then(|| "was halfway through refactoring the loader".to_string()),
                },
                command_builder(),
            )
            .await
            .unwrap_or_else(|e| panic!("purpose {purpose:?} failed to open: {e}"));

        assert!(
            handle.prelude.rules.contains(&"Always answer in Chinese".to_string()),
            "purpose {purpose:?} lost the user rules"
        );
        assert!(
            !handle.prelude.memories.is_empty(),
            "purpose {purpose:?} lost the recalled memory"
        );
        if matches!(purpose, SessionPurpose::RelaunchForConfig) {
            assert!(
                handle.prelude.memories.iter().any(|m| m.contains("refactoring the loader")),
                "a relaunch must carry the handoff summary; there is no in-place switch"
            );
        }
    }
}

#[test]
fn the_prelude_puts_rules_above_memories_and_never_annotates_them() {
    let p = Prelude {
        rules: vec!["Ask before adding a dependency".into()],
        memories: vec!["[confidence 0.6 | run 412] the build uses cargo".into()],
    };
    let text = p.render();

    let rule_at = text.find("Ask before adding").unwrap();
    let memory_at = text.find("the build uses cargo").unwrap();
    assert!(rule_at < memory_at, "rules must come first");

    let rules_heading = text.find("User rules").unwrap();
    let memories_heading = text.find("Recalled context").unwrap();
    assert!(rules_heading < rule_at && rule_at < memories_heading);

    // The rule line itself carries no confidence marker. A rule is an instruction, not a
    // claim, and annotating it invites the model to weigh it against the evidence.
    let rule_line = text.lines().find(|l| l.contains("Ask before adding")).unwrap();
    assert!(!rule_line.contains("confidence"));
    assert!(text.contains("the rule wins"), "conflict resolution must be stated");
}

#[test]
fn empty_prelude_renders_nothing() {
    assert!(Prelude::default().render().is_empty());
}

/// Conformance: the messages we construct must be what the protocol actually specifies.
///
/// Hand-rolled outgoing JSON is fast and tolerant, but it can drift from the spec silently.
/// Deserializing our own output into the official crate's types catches that drift at test
/// time rather than against a real agent.
#[test]
fn outgoing_messages_match_the_official_schema() {
    use agent_client_protocol::schema::v1;

    let initialize = json!({ "protocolVersion": 1 });
    serde_json::from_value::<v1::InitializeRequest>(initialize)
        .expect("our initialize params must satisfy the official schema");

    let new_session = json!({ "cwd": "/tmp/project", "mcpServers": [] });
    serde_json::from_value::<v1::NewSessionRequest>(new_session)
        .expect("our session/new params must satisfy the official schema");

    let prompt = json!({
        "sessionId": "s-1",
        "prompt": [{ "type": "text", "text": "hello" }]
    });
    serde_json::from_value::<v1::PromptRequest>(prompt)
        .expect("our session/prompt params must satisfy the official schema");

    let set_config = json!({ "sessionId": "s-1", "configId": "model", "value": "gpt" });
    serde_json::from_value::<v1::SetSessionConfigOptionRequest>(set_config)
        .expect("our set_config_option params must satisfy the official schema");

    let cancel = json!({ "sessionId": "s-1" });
    serde_json::from_value::<v1::CancelNotification>(cancel)
        .expect("our session/cancel params must satisfy the official schema");

    let permission_reply = json!({
        "outcome": { "outcome": "selected", "optionId": "allow-once" }
    });
    serde_json::from_value::<v1::RequestPermissionResponse>(permission_reply)
        .expect("our permission response must satisfy the official schema");
}

/// The reverse direction: the shapes we claim to parse must be shapes the official types
/// can produce, so the tolerant parser is tolerant about the right things.
#[test]
fn incoming_shapes_we_parse_are_shapes_the_schema_can_produce() {
    use agent_client_protocol::schema::v1;

    let thought = json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": "hm" },
        "messageId": "m1"
    });
    serde_json::from_value::<v1::SessionUpdate>(thought.clone())
        .expect("the schema must accept a thought chunk with a messageId");
    match wire::map_session_update(&thought, false) {
        RawUpdate::TextChunk { kind, message_id, text } => {
            assert_eq!(kind, SegmentKind::Thought);
            assert_eq!(message_id.as_deref(), Some("m1"));
            assert_eq!(text, "hm");
        }
        other => panic!("expected a text chunk, got {other:?}"),
    }

    // messageId omitted, which v1 permits and which is the case that forces the fallback.
    let no_id = json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": "hm" }
    });
    serde_json::from_value::<v1::SessionUpdate>(no_id.clone())
        .expect("the schema must accept a chunk without a messageId");
    assert!(matches!(
        wire::map_session_update(&no_id, false),
        RawUpdate::TextChunk { message_id: None, .. }
    ));

    let usage = json!({ "sessionUpdate": "usage_update", "used": 10, "size": 100 });
    serde_json::from_value::<v1::SessionUpdate>(usage.clone())
        .expect("the schema must accept a usage update");
    assert!(matches!(
        wire::map_session_update(&usage, false),
        RawUpdate::Usage { used: 10, size: 100, .. }
    ));

    let diff = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t1",
        "status": "completed",
        "content": [{ "type": "diff", "path": "/a", "oldText": "x", "newText": "y" }]
    });
    serde_json::from_value::<v1::SessionUpdate>(diff.clone())
        .expect("the schema must accept diff content on a tool call");
    match wire::map_session_update(&diff, false) {
        RawUpdate::ToolCallUpdate { content, .. } => {
            assert!(matches!(content[0], ToolContent::Diff { .. }));
        }
        other => panic!("expected a tool call update, got {other:?}"),
    }
}

#[test]
fn an_incomplete_usage_update_is_not_treated_as_usage() {
    // Reporting `used` with no `size` gives a numerator and no denominator. Inventing the
    // denominator would put a percentage on screen that means nothing.
    let partial = json!({ "sessionUpdate": "usage_update", "used": 10 });
    assert!(matches!(
        wire::map_session_update(&partial, false),
        RawUpdate::Unknown { .. }
    ));
}
