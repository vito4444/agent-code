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
    /// Enforces the boundary for file access performed on the agent's behalf. `None` means the
    /// capability was not offered, so a request for it is a protocol error rather than a refusal.
    pub guard: Option<Arc<wkbd_sec::path_guard::PathGuard>>,
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
    pub(crate) rx: tokio::sync::oneshot::Receiver<Option<String>>,
    pub(crate) timeout: std::time::Duration,
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
        // Wait on the connection's delivery watermark rather than on our own progress. One agent
        // process serves several sessions — the pool keys on agent plus launch settings, not on
        // session — so the lines between our last notification and our response can belong to
        // somebody else entirely. Measuring our own progress against a position on the shared
        // stream then waits for messages that will never arrive in our inbox, and every turn pays
        // the full timeout. That failure needs two concurrent sessions on one process to appear,
        // which is why it survived a suite that opens one at a time.
        if !ctx
            .handle
            .connection()
            .wait_routed(target, std::time::Duration::from_secs(10))
            .await
        {
            tracing::warn!(target, "timed out waiting for the stream to catch up to the response");
        }
        // Everything up to the response has been placed in some session's inbox, so anything of
        // ours is already queued and this drain cannot block.
        while let Ok(msg) = rx.try_recv() {
            highest_processed = highest_processed.max(msg.read_seq());
            emit(handle_incoming(ctx, msg).await).await;
        }
        let deadline = tokio::time::Instant::now();
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
            "fs/read_text_file" | "fs/write_text_file" => {
                handle_fs(ctx, id, &method, params).await
            }
            // terminal/* is the other interface where an unsandboxed client acts for the agent.
            // Unlike fs/*, there is nothing behind it yet: a terminal means spawning a process
            // with an agent-chosen command line, and the process supervisor is wired for agents
            // rather than for arbitrary commands. Refusing is honest; the agent runs the command
            // itself, in its own sandbox, exactly as it would if we had never offered.
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

/// Performs a file operation on the agent's behalf, records it, and answers.
///
/// The boundary decision is entirely `wkbd-sec::path_guard`'s; nothing here re-implements any part
/// of it. What happens here is the conversion between a protocol request and an audit record, and
/// the one policy decision that cannot live in the guard: a request for a capability we never
/// offered is a protocol error, not a refusal, because the agent should not have asked.
async fn handle_fs(
    ctx: &TurnContext,
    id: Value,
    method: &str,
    params: Value,
) -> Vec<EventPayload> {
    let Some(guard) = &ctx.guard else {
        let _ = ctx
            .handle
            .connection()
            .respond_error(
                &id,
                -32601,
                &format!("{method} was not offered by this client"),
            )
            .await;
        return Vec::new();
    };

    let outcome = if method == "fs/read_text_file" {
        crate::fs_bridge::read_text_file(guard, &params)
    } else {
        crate::fs_bridge::write_text_file(guard, &params)
    };

    match outcome.reply {
        Ok(result) => {
            if let Err(e) = ctx.handle.connection().respond(&id, result).await {
                tracing::warn!(error = %e, "could not answer a file request");
            }
        }
        Err((code, message)) => {
            if let Err(e) = ctx.handle.connection().respond_error(&id, code, &message).await {
                tracing::warn!(error = %e, "could not refuse a file request");
            }
        }
    }

    vec![outcome.event]
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

/// Answers a worker's permission requests without asking anyone.
///
/// Necessary and worth being blunt about. A worker dispatched by the orchestrator has nobody
/// watching it: waiting for a human would stall every parallel run on its first tool call, and the
/// wait would time out into a refusal, so "ask" and "refuse everything" are the same policy in
/// practice. Refusing everything makes a worker that cannot do its job.
///
/// So what actually constrains a worker is not the prompt. It is the worktree it runs in, the path
/// guard rooted at that worktree, and the acceptance check on its output. Those hold whether or not
/// it was asked. The prompt is a UI affordance for an interactive session, and treating it as a
/// security boundary for an unattended one would be believing a check that nobody is performing.
///
/// Every request still reaches the event log through the normal path, so a run's transcript shows
/// what was asked and that it was allowed automatically.
pub struct AutoAllow;

#[async_trait::async_trait]
impl AskUser for AutoAllow {
    async fn register(&self, _request_id: &str) -> PermissionWaiter {
        // Answered before it is awaited, so the worker never blocks. `None` would mean refuse.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Some(AUTO_ALLOW_OPTION.to_string()));
        PermissionWaiter { rx, timeout: std::time::Duration::from_secs(1) }
    }

    async fn cancel(&self, _request_id: &str) {}
}

/// The option id reported for an automatic allowance.
///
/// A distinct, obviously-not-a-user string rather than reusing whatever the agent offered: a
/// transcript reader has to be able to tell an automatic decision from one a person made, and a
/// remembered decision that came from nobody must never be replayed as though somebody chose it.
pub const AUTO_ALLOW_OPTION: &str = "wkbd:auto-allow-unattended";
