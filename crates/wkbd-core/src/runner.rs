//! The turn runner.
//!
//! One place drives a prompt turn from start to finish, because getting the ordering
//! slightly wrong is both easy and invisible. Two rules that this module exists to hold:
//!
//! - **The only authoritative end of a turn is the `session/prompt` response returning.**
//!   The stream going quiet means the model paused between tool calls, which looks
//!   identical from outside and is not the same thing. Anything that queues user input
//!   until "the turn ends" depends on this distinction.
//! - **When that response arrives, notifications sent before it may still be buffered.**
//!   Ending the turn without draining them loses the tail of the transcript, usually the
//!   final answer and the last tool result. This was found three separate times in
//!   development, in the tests, in the capability probe and here, which is why the turn
//!   loop now lives in exactly one place.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use wkbd_agent::{Incoming, ProcessKey, SessionHandle};
use wkbd_proto::{EventPayload, PendingEvent, StopReason};
use wkbd_sec::permission::{Decision, PermissionKey, PermissionStore, Scope};
use wkbd_store::Store;

/// Everything a running turn needs. Deliberately not the whole daemon state: a turn should
/// not be able to reach the orchestrator or the process pool.
pub struct TurnContext {
    pub store: Store,
    pub session_local_id: String,
    pub handle: Arc<SessionHandle>,
    pub permissions: Arc<PermissionStore>,
    pub project_root: String,
    /// Asks the user. Returns the chosen option id, or `None` to cancel. `None` is also
    /// what an unattended daemon returns, which is why the default answer is refusal
    /// rather than approval.
    pub ask_user: Arc<dyn AskUser>,
}

#[async_trait::async_trait]
pub trait AskUser: Send + Sync {
    async fn ask(
        &self,
        session_id: &str,
        request_id: &str,
        title: &str,
        options: &[wkbd_proto::PermissionOption],
    ) -> Option<String>;
}

/// Runs one turn to completion, persisting every event as it happens.
///
/// `rx` must deliver only messages for this session's process. The caller demultiplexes.
pub async fn run_turn(
    ctx: &TurnContext,
    prompt: &str,
    rx: &mut mpsc::UnboundedReceiver<(ProcessKey, Incoming)>,
) -> Result<StopReason> {
    let mut emit = |payloads: Vec<EventPayload>| {
        let store = ctx.store.clone();
        let sid = ctx.session_local_id.clone();
        async move {
            if payloads.is_empty() {
                return;
            }
            let pending: Vec<PendingEvent> =
                payloads.into_iter().map(|p| PendingEvent::new(sid.clone(), p)).collect();
            if let Err(e) = store.append(pending).await {
                // A failed append must not abort the turn: the agent is already working and
                // losing the log is better than losing the work.
                tracing::error!(error = %e, "could not persist turn events");
            }
        }
    };

    emit(ctx.handle.begin_turn(prompt).await).await;

    let conn = ctx.handle.connection();
    let session_id = ctx.handle.acp_session_id.clone();
    let prelude = ctx.handle.prelude.render();
    let mut blocks = Vec::new();
    if !prelude.is_empty() {
        blocks.push(json!({ "type": "text", "text": prelude }));
    }
    blocks.push(json!({ "type": "text", "text": prompt }));

    let prompt_task = tokio::spawn(async move {
        conn.request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": blocks }),
        )
        .await
    });
    let mut prompt_task = prompt_task;

    let stop_reason;
    loop {
        tokio::select! {
            res = &mut prompt_task => {
                stop_reason = match res {
                    Ok(Ok(v)) => wkbd_agent::wire::parse_stop_reason(
                        v.get("stopReason").and_then(|s| s.as_str()),
                    ),
                    Ok(Err(e)) => {
                        emit(vec![EventPayload::AgentError { message: e.to_string() }]).await;
                        StopReason::Unknown
                    }
                    Err(e) => {
                        emit(vec![EventPayload::AgentError {
                            message: format!("turn task failed: {e}"),
                        }])
                        .await;
                        StopReason::Unknown
                    }
                };
                break;
            }
            msg = rx.recv() => {
                let Some((_, msg)) = msg else {
                    emit(vec![EventPayload::AgentExited { code: None, signal: None }]).await;
                    stop_reason = StopReason::Unknown;
                    break;
                };
                emit(handle_incoming(ctx, msg).await).await;
            }
        }
    }

    // See the module comment. This drain is load-bearing, not defensive.
    while let Ok((_, msg)) = rx.try_recv() {
        emit(handle_incoming(ctx, msg).await).await;
    }

    emit(ctx.handle.end_turn(stop_reason).await).await;
    Ok(stop_reason)
}

async fn handle_incoming(ctx: &TurnContext, msg: Incoming) -> Vec<EventPayload> {
    match msg {
        Incoming::Notification { method, params } if method == "session/update" => {
            match params.get("update") {
                Some(update) => ctx.handle.ingest_update(update).await,
                None => Vec::new(),
            }
        }
        Incoming::Notification { .. } => Vec::new(),
        Incoming::Request { id, method, params, .. } => match method.as_str() {
            "session/request_permission" => handle_permission(ctx, id, params).await,
            // fs/* and terminal/* are the interfaces where an unsandboxed client executes
            // work on the agent's behalf, which makes them the shortest path around every
            // permission check in the system. They are refused until the path guard and the
            // sandbox are wired in, and refusing is the safe default: an agent that cannot
            // read a file through us will read it itself, inside its own sandbox.
            other => {
                let _ = ctx
                    .handle
                    .connection()
                    .respond_error(
                        &id,
                        -32601,
                        &format!("{other} is not offered by this client"),
                    )
                    .await;
                Vec::new()
            }
        },
    }
}

async fn handle_permission(ctx: &TurnContext, id: Value, params: Value) -> Vec<EventPayload> {
    let request_id = format!("{id}");
    let (mut out, options) = ctx.handle.note_permission_request(&request_id, &params).await;

    // The remembered decision is keyed on the kind of operation plus a hash of what the
    // operation actually is. Keying on a name or an id instead is how an approved entry
    // gets swapped for a different payload without re-prompting.
    let tool_kind = params
        .get("toolCall")
        .and_then(|t| t.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let decision_bytes = canonical_decision_bytes(&params);
    let key = PermissionKey {
        tool_kind,
        content_hash: PermissionStore::hash_decision_content(&[&decision_bytes]),
    };

    let remembered = ctx
        .permissions
        .lookup(Scope::Project(ctx.project_root.clone()), &key)
        .or_else(|| ctx.permissions.lookup(Scope::Global, &key));

    let (chosen, auto) = match remembered {
        Some(Decision::Deny) => (None, true),
        Some(Decision::Allow) => (
            options
                .iter()
                .find(|o| {
                    matches!(
                        o.kind,
                        wkbd_proto::PermissionOptionKind::AllowOnce
                            | wkbd_proto::PermissionOptionKind::AllowAlways
                    )
                })
                .map(|o| o.option_id.clone()),
            true,
        ),
        None => {
            let answer = ctx
                .ask_user
                .ask(&ctx.session_local_id, &request_id, "Permission required", &options)
                .await;
            // "Always" answers are remembered against the content hash, so the same
            // decision on different content asks again.
            if let Some(picked) = &answer {
                if let Some(opt) = options.iter().find(|o| &o.option_id == picked) {
                    match opt.kind {
                        wkbd_proto::PermissionOptionKind::AllowAlways => {
                            ctx.permissions.remember(
                                Scope::Project(ctx.project_root.clone()),
                                key.clone(),
                                Decision::Allow,
                            );
                        }
                        wkbd_proto::PermissionOptionKind::RejectAlways => {
                            ctx.permissions.remember(
                                Scope::Project(ctx.project_root.clone()),
                                key.clone(),
                                Decision::Deny,
                            );
                        }
                        _ => {}
                    }
                }
            }
            (answer, false)
        }
    };

    let reply = match &chosen {
        Some(option_id) => json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    };
    if let Err(e) = ctx.handle.connection().respond(&id, reply).await {
        tracing::warn!(error = %e, "could not answer a permission request");
    }

    out.extend(ctx.handle.note_permission_resolved(&request_id, chosen, auto).await);
    out
}

/// Canonical bytes for a permission decision.
///
/// Everything that determines what will actually happen goes in, with a stable ordering, so
/// that a change of one byte in a command, an argument or a path invalidates a previously
/// remembered approval.
fn canonical_decision_bytes(params: &Value) -> Vec<u8> {
    let tool = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let mut parts = Vec::new();
    for key in ["kind", "title", "rawInput"] {
        parts.push(format!("{key}={}", tool.get(key).unwrap_or(&Value::Null)));
    }
    if let Some(locations) = tool.get("locations").and_then(|l| l.as_array()) {
        let mut paths: Vec<String> = locations
            .iter()
            .filter_map(|l| l.get("path").and_then(|p| p.as_str()).map(str::to_string))
            .collect();
        paths.sort();
        parts.push(format!("locations={}", paths.join(",")));
    }
    parts.join("\u{1f}").into_bytes()
}
