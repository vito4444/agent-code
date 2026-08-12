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
use wkbd_agent::{Incoming, SessionHandle};
use wkbd_proto::{EventPayload, PendingEvent, StopReason};
use wkbd_sec::permission::{hash_decision_content, Decision, PermissionKey, PermissionStore, Scope};
use wkbd_store::Store;

/// Everything a running turn needs. Deliberately not the whole daemon state: a turn should
/// not be able to reach the orchestrator or the process pool.
pub struct TurnContext {
    pub store: Store,
    /// Publishes persisted events to connected clients. Called only after the append
    /// succeeds, so a client can never see an event that is not in the log — the log is what
    /// a reconnecting client resumes from, and an event that exists only in a live stream
    /// would vanish on reconnect.
    pub publish: Arc<dyn Fn(&[wkbd_proto::Event]) + Send + Sync>,
    pub session_local_id: String,
    pub handle: Arc<SessionHandle>,
    pub permissions: Arc<tokio::sync::Mutex<PermissionStore>>,
    pub project_root: String,
    /// Asks the user. Returns the chosen option id, or `None` to cancel. `None` is also
    /// what an unattended daemon returns, which is why the default answer is refusal
    /// rather than approval.
    pub ask_user: Arc<dyn AskUser>,
}

/// Routes a permission question to whoever answers it.
///
/// Split into `register` and awaiting the returned future so the caller can register before
/// announcing the request. Combining them into one `ask` call makes it impossible to close the
/// window in which an answer arrives with nothing to receive it.
#[async_trait::async_trait]
pub trait AskUser: Send + Sync {
    /// Records that an answer is expected, and returns something to await it on.
    async fn register(&self, request_id: &str) -> PermissionWaiter;
    /// Withdraws a registration, for when a remembered decision means nobody needs to be asked.
    async fn cancel(&self, request_id: &str);
}

/// Awaits a permission answer. `None` means refuse.
pub struct PermissionWaiter {
    rx: tokio::sync::oneshot::Receiver<Option<String>>,
    timeout: std::time::Duration,
}

impl PermissionWaiter {
    pub fn new(
        rx: tokio::sync::oneshot::Receiver<Option<String>>,
        timeout: std::time::Duration,
    ) -> Self {
        Self { rx, timeout }
    }
}

impl std::future::IntoFuture for PermissionWaiter {
    type Output = Option<String>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // Timing out refuses rather than approves. An unattended workbench must not become
            // an approving one, and the agent gets a definite answer either way instead of
            // blocking on a human who is not there.
            match tokio::time::timeout(self.timeout, self.rx).await {
                Ok(Ok(answer)) => answer,
                _ => None,
            }
        })
    }
}

/// Runs one turn to completion, persisting every event as it happens.
///
/// `rx` must deliver only messages for this session's process. The caller demultiplexes.
pub async fn run_turn(
    ctx: &TurnContext,
    prompt: &str,
    rx: &mut mpsc::UnboundedReceiver<Incoming>,
) -> Result<StopReason> {
    let emit = |payloads: Vec<EventPayload>| {
        let store = ctx.store.clone();
        let sid = ctx.session_local_id.clone();
        let publish = ctx.publish.clone();
        async move {
            if payloads.is_empty() {
                return;
            }
            let pending: Vec<PendingEvent> =
                payloads.into_iter().map(|p| PendingEvent::new(sid.clone(), p)).collect();
            match store.append(pending).await {
                Ok(written) => publish(&written),
                Err(e) => {
                    // A failed append must not abort the turn: the agent is already working,
                    // and losing part of the log is less bad than losing the work. Nothing is
                    // published in this case, so clients never see an event the log lacks.
                    tracing::error!(error = %e, "could not persist turn events");
                }
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
    // The read position of the line that carried the prompt response. Everything the agent
    // wrote before that position belongs to this turn and must be processed before it ends.
    let mut response_read_seq: Option<u64> = None;
    let mut highest_processed: u64 = 0;
    let mut agent_gone = false;

    loop {
        tokio::select! {
            res = &mut prompt_task => {
                stop_reason = match res {
                    Ok(Ok(response)) => {
                        // What the connection had dispatched by the time the response landed.
                        // Exact, unlike inferring it from the response's own position: not every
                        // line before a response is a notification.
                        response_read_seq = Some(ctx.handle.connection().dispatched_seq());
                        let _ = response.read_seq;
                        wkbd_agent::wire::parse_stop_reason(
                            response.get("stopReason").and_then(|s| s.as_str()),
                        )
                    }
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
                let Some(msg) = msg else {
                    emit(vec![EventPayload::AgentExited { code: None, signal: None }]).await;
                    stop_reason = StopReason::Unknown;
                    break;
                };
                highest_processed = highest_processed.max(msg.read_seq());
                emit(handle_incoming(ctx, msg).await).await;
            }
        }
    }

    // Catch up to the response.
    //
    // A response is handed straight to whoever awaits it; a notification goes through the
    // pool's channel and a dispatcher first. The response therefore arrives over a shorter path
    // and routinely overtakes notifications the agent emitted before it. Ending the turn here
    // would drop the tail of the transcript — the final answer and the last tool result — and it
    // would do so depending on scheduling, which is why it presents as an intermittent bug.
    //
    // Waiting on the read position rather than on a timer makes this deterministic: we know
    // exactly how many lines came before the response, so we know when we have them all.
    if let Some(target) = response_read_seq {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while highest_processed < target && !agent_gone {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // The bound exists because a line can be unparseable and therefore never
                // dispatched, which would otherwise leave this loop waiting for a message that
                // does not exist. Reaching it is worth knowing about.
                tracing::warn!(
                    highest_processed,
                    target,
                    "gave up waiting for notifications that preceded the prompt response"
                );
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(msg)) => {
                    highest_processed = highest_processed.max(msg.read_seq());
                    emit(handle_incoming(ctx, msg).await).await;
                }
                Ok(None) => agent_gone = true,
                Err(_) => continue,
            }
        }
    }

    // Anything already queued beyond the response, such as an update the agent sent after
    // answering, is taken too rather than left for the next turn to misattribute.
    while let Ok(msg) = rx.try_recv() {
        emit(handle_incoming(ctx, msg).await).await;
    }

    emit(ctx.handle.end_turn(stop_reason).await).await;
    Ok(stop_reason)
}

async fn handle_incoming(ctx: &TurnContext, msg: Incoming) -> Vec<EventPayload> {
    match msg {
        Incoming::Notification { method, params, .. } if method == "session/update" => {
            match params.get("update") {
                Some(update) => ctx.handle.ingest_update(update).await,
                None => Vec::new(),
            }
        }
        Incoming::Notification { .. } => Vec::new(),
        Incoming::Request { id, method, params, .. } => match method.as_str() {
            "session/request_permission" => begin_permission(ctx, id, params).await,
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

/// Starts a permission exchange and returns immediately.
///
/// Two orderings here are load-bearing, and getting either wrong produces a deadlock rather
/// than a wrong answer.
///
/// **The waiter is registered before the request is announced.** A client learns about the
/// request from the event log, so registering first means an answer can never arrive before
/// there is anything to receive it. Announcing first leaves a window in which the answer is
/// dropped and the turn then waits for an answer that has already been given.
///
/// **The wait happens on its own task.** Awaiting the human inside the turn's event loop stops
/// every other notification from being processed — including the event that tells the client
/// there is something to answer. That is a deadlock by construction: the client waits for the
/// event, and the daemon waits for the client.
async fn begin_permission(ctx: &TurnContext, id: Value, params: Value) -> Vec<EventPayload> {
    let request_id = format!("{id}");

    // Registered first. See above.
    let waiter = ctx.ask_user.register(&request_id).await;

    let (out, options) = ctx.handle.note_permission_request(&request_id, &params).await;

    let tool_kind = params
        .get("toolCall")
        .and_then(|t| t.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let decision_bytes = canonical_decision_bytes(&params);
    let key = PermissionKey::new(tool_kind, hash_decision_content(&[&decision_bytes]));
    let scope = Scope::session(ctx.project_root.clone(), ctx.session_local_id.clone());

    // A remembered decision short-circuits the human. `resolve` walks session, then project,
    // then global, and a deny anywhere on that chain wins: a deny that a narrower allow could
    // override is advisory, not a boundary.
    let remembered = ctx.permissions.lock().await.lookup(&scope, &key);

    let store = ctx.store.clone();
    let publish = ctx.publish.clone();
    let session_local_id = ctx.session_local_id.clone();
    let handle = ctx.handle.clone();
    let permissions = ctx.permissions.clone();
    let project_root = ctx.project_root.clone();
    let ask_user = ctx.ask_user.clone();

    tokio::spawn(async move {
        let (chosen, auto) = match remembered {
            Some(Decision::Deny) => {
                ask_user.cancel(&request_id).await;
                (None, true)
            }
            Some(Decision::Allow) => {
                ask_user.cancel(&request_id).await;
                (
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
                )
            }
            None => {
                let answer = waiter.await;
                if let Some(picked) = &answer {
                    if let Some(opt) = options.iter().find(|o| &o.option_id == picked) {
                        let decision = match opt.kind {
                            wkbd_proto::PermissionOptionKind::AllowAlways => Some(Decision::Allow),
                            wkbd_proto::PermissionOptionKind::RejectAlways => Some(Decision::Deny),
                            _ => None,
                        };
                        if let Some(decision) = decision {
                            // Remembered against the project and against the content hash. The
                            // same kind of operation on different content asks again, which is
                            // what stops an approved decision covering a different payload.
                            permissions.lock().await.remember(
                                Scope::project(project_root.clone()),
                                key.clone(),
                                decision,
                            );
                        }
                    }
                }
                (answer, false)
            }
        };

        let reply = match &chosen {
            Some(option_id) => {
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            }
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        if let Err(e) = handle.connection().respond(&id, reply).await {
            tracing::warn!(error = %e, "could not answer a permission request");
        }

        let resolved = handle.note_permission_resolved(&request_id, chosen, auto).await;
        if !resolved.is_empty() {
            let pending: Vec<PendingEvent> = resolved
                .into_iter()
                .map(|p| PendingEvent::new(session_local_id.clone(), p))
                .collect();
            match store.append(pending).await {
                Ok(written) => publish(&written),
                Err(e) => tracing::error!(error = %e, "could not persist the permission outcome"),
            }
        }
    });

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
