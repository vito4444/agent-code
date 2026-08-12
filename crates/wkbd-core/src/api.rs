//! HTTP and WebSocket surface.
//!
//! The interface is served by the daemon over loopback rather than only through a desktop
//! shell's embedded webview. That is deliberate: the embedded webview is the least
//! predictable part of the stack on Linux, and serving the same build over HTTP means a
//! browser is a working escape route rather than a rewrite. It is also what makes remote
//! access and headless use fall out for free instead of needing a second implementation.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use wkbd_agent::SessionPurpose;
use wkbd_memory::rules::{NewRule, RuleScope};

use crate::runner::{run_turn, AskUser, TurnContext};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/agents", get(list_agents))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/prompt", post(prompt))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/permission", post(answer_permission))
        .route("/api/sessions/{id}/config", post(set_config))
        .route("/api/terminals/{id}", get(terminal_output))
        .route("/api/rules", get(list_rules).post(create_rule))
        .route("/api/rules/{id}", delete(delete_rule))
        .route("/api/raw", get(raw_frames))
        .route("/api/stream", get(stream))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "read_only": state.store.is_read_only(),
        "degraded": state.degraded,
        "agents": state.agents.len(),
    }))
}

async fn list_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(
        state
            .agents
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "display_name": a.display_name,
                    // Which settings this agent can change without restarting. The interface
                    // needs this to decide whether a selector switches in place or opens a
                    // new session, and there is no way to ask the protocol.
                    "live_config_ids": a.live_config_ids,
                })
            })
            .collect::<Vec<_>>(),
    )
}

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    agent_id: String,
    agent_display_name: String,
    project_root: String,
    title: Option<String>,
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let out: Vec<SessionSummary> = sessions
        .values()
        .map(|s| SessionSummary {
            id: s.handle.local_id.clone(),
            agent_id: s.agent_id.clone(),
            agent_display_name: s.agent_display_name.clone(),
            project_root: s.project_root.clone(),
            title: s.title.clone(),
        })
        .collect();
    Json(out)
}

#[derive(Deserialize)]
struct CreateSession {
    agent_id: String,
    project_root: String,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSession>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state
        .open_session(&body.agent_id, &body.project_root, SessionPurpose::NewChat)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(SessionSummary {
        id: session.handle.local_id.clone(),
        agent_id: session.agent_id.clone(),
        agent_display_name: session.agent_display_name.clone(),
        project_root: session.project_root.clone(),
        title: session.title.clone(),
    }))
}

#[derive(Deserialize)]
struct PromptBody {
    text: String,
}

async fn prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PromptBody>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state.session(&id).await.ok_or_else(ApiError::not_found)?;

    {
        // One turn at a time per session. The protocol has no way to inject into a running
        // turn, so a second concurrent prompt would either be silently queued by the agent
        // or interleave unpredictably.
        let mut busy = session.busy.lock().await;
        if *busy {
            return Err(ApiError::conflict("this session is already running a turn"));
        }
        *busy = true;
    }

    let state2 = state.clone();
    let session2 = session.clone();
    let text = body.text.clone();
    tokio::spawn(async move {
        let ctx = TurnContext {
            store: state2.store.clone(),
            session_local_id: session2.handle.local_id.clone(),
            handle: session2.handle.clone(),
            permissions: state2.permissions.clone(),
            project_root: session2.project_root.clone(),
            ask_user: Arc::new(PendingPrompt::new(state2.clone())),
        };
        let mut inbox = session2.inbox.lock().await;
        if let Err(e) = run_turn(&ctx, &text, &mut inbox).await {
            tracing::error!(error = %e, "turn failed");
        }
        *session2.busy.lock().await = false;
    });

    Ok(StatusCode::ACCEPTED)
}

async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state.session(&id).await.ok_or_else(ApiError::not_found)?;
    session.handle.cancel().await.map_err(ApiError::internal)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct PermissionAnswer {
    request_id: String,
    option_id: Option<String>,
}

async fn answer_permission(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PermissionAnswer>,
) -> Result<impl IntoResponse, ApiError> {
    state.session(&id).await.ok_or_else(ApiError::not_found)?;
    let delivered = state
        .deliver_permission_answer(&body.request_id, body.option_id)
        .await;
    if delivered {
        Ok(StatusCode::ACCEPTED)
    } else {
        // The request is gone, which usually means the turn was cancelled while the prompt
        // was on screen. Saying so is better than accepting an answer nobody will read.
        Err(ApiError::conflict("that permission request is no longer waiting"))
    }
}

#[derive(Deserialize)]
struct ConfigBody {
    option_id: String,
    value: serde_json::Value,
}

async fn set_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ConfigBody>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state.session(&id).await.ok_or_else(ApiError::not_found)?;
    let applied = session
        .handle
        .set_config_option(&body.option_id, body.value)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(json!({
        "applied": applied.is_some(),
        // The honest answer when the agent refuses. There is no in-place switch for an agent
        // that reads its configuration once at startup, so the caller has to open a new
        // session and hand it a summary rather than pretend the change took effect.
        "requires_new_session": applied.is_none(),
        "config_options": applied,
    })))
}

async fn terminal_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Terminals belong to whichever agent created them. Until the client-side terminal
    // capability is offered, there is nothing to read, and saying so is better than
    // returning an empty string that looks like a command with no output.
    let _ = (&state, &id);
    Err(ApiError::not_implemented(
        "this client does not offer terminal/* yet, so there is no output to read",
    ))
}

#[derive(Deserialize)]
struct RuleQuery {
    project_root: Option<String>,
}

async fn list_rules(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RuleQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let rules = wkbd_memory::rules::list(&state.store, q.project_root.as_deref())
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(rules))
}

#[derive(Deserialize)]
struct CreateRule {
    scope: String,
    project_root: Option<String>,
    body: String,
}

async fn create_rule(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRule>,
) -> Result<impl IntoResponse, ApiError> {
    let scope = match body.scope.as_str() {
        "global" => RuleScope::Global,
        "project" => RuleScope::Project(
            body.project_root
                .clone()
                .ok_or_else(|| ApiError::bad_request("a project rule needs a project root"))?,
        ),
        other => return Err(ApiError::bad_request(&format!("unknown scope: {other}"))),
    };

    let rule = wkbd_memory::rules::create(&state.store, NewRule { scope, body: body.body })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(rule))
}

async fn delete_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    wkbd_memory::rules::delete(&state.store, &id)
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RawQuery {
    limit: Option<usize>,
}

async fn raw_frames(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RawQuery>,
) -> impl IntoResponse {
    let log = state.raw.lock().await;
    let limit = q.limit.unwrap_or(500).min(5_000);
    let start = log.len().saturating_sub(limit);
    Json(log[start..].to_vec())
}

/* ------------------------------------------------------------------ streaming */

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Subscribe { since_seq: i64 },
}

async fn stream(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_stream(state, socket))
}

async fn handle_stream(state: Arc<AppState>, mut socket: WebSocket) {
    let hwm = state.store.high_water_mark().unwrap_or(0);
    let hello = json!({
        "type": "hello",
        "degraded": state.degraded,
        "high_water_mark": hwm,
    });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }

    // Subscribe before replaying history, so an event that lands during the replay is
    // buffered by the broadcast channel rather than lost between the two.
    let mut rx = state.events.subscribe();

    let mut since = 0i64;
    if let Some(Ok(Message::Text(text))) = socket.recv().await {
        if let Ok(ClientFrame::Subscribe { since_seq }) = serde_json::from_str(&text) {
            since = since_seq;
        }
    }

    // Replay in chunks so a long history does not build one enormous frame.
    loop {
        let (events, _) = match state.store.read_since(None, since, 500) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "history replay failed");
                break;
            }
        };
        if events.is_empty() {
            break;
        }
        since = events.last().map(|e| e.seq).unwrap_or(since);
        let frame = json!({ "type": "events", "events": events });
        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
            return;
        }
    }

    let mut idle = tokio::time::interval(std::time::Duration::from_millis(400));
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sent_since_tick = false;

    loop {
        tokio::select! {
            received = rx.recv() => {
                match received {
                    Ok(event) => {
                        if event.seq <= since { continue; }
                        since = event.seq;
                        let frame = json!({ "type": "events", "events": [event] });
                        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                            return;
                        }
                        sent_since_tick = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // The client fell behind the broadcast buffer. Rather than guess at
                        // what it missed, tell it to resume from the log by sequence number,
                        // which is the one mechanism that cannot lose an event.
                        tracing::warn!(skipped, "stream lagged; replaying from the log");
                        if let Ok((events, _)) = state.store.read_since(None, since, 2_000) {
                            if let Some(last) = events.last() { since = last.seq; }
                            let frame = json!({ "type": "events", "events": events });
                            if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
            _ = idle.tick() => {
                // Tells the client the stream has gone quiet so it can flush its frame
                // buffer. Without this the last few events of a turn sit in the buffer
                // waiting for an animation frame that is never requested.
                if sent_since_tick {
                    sent_since_tick = false;
                    let _ = socket.send(Message::Text(json!({ "type": "idle" }).to_string().into())).await;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Err(_)) => return,
                    Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/* -------------------------------------------------------------------- errors */

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(e: impl std::fmt::Display) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: e.to_string() }
    }
    fn not_found() -> Self {
        Self { status: StatusCode::NOT_FOUND, message: "no such session".into() }
    }
    fn conflict(msg: &str) -> Self {
        Self { status: StatusCode::CONFLICT, message: msg.into() }
    }
    fn bad_request(msg: &str) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }
    fn not_implemented(msg: &str) -> Self {
        Self { status: StatusCode::NOT_IMPLEMENTED, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/* ------------------------------------------------------- permission plumbing */

/// Bridges a permission request from a turn to whichever client answers it.
struct PendingPrompt {
    state: Arc<AppState>,
}

impl PendingPrompt {
    fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl AskUser for PendingPrompt {
    async fn ask(
        &self,
        _session_id: &str,
        request_id: &str,
        _title: &str,
        options: &[wkbd_proto::PermissionOption],
    ) -> Option<String> {
        let rx = self.state.register_permission_wait(request_id).await;
        match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
            Ok(Ok(answer)) => answer,
            // Timing out refuses rather than approves. An unattended workbench must not
            // become an approving one, and the agent gets a definite answer either way
            // rather than blocking forever.
            _ => {
                self.state.clear_permission_wait(request_id).await;
                let _ = options;
                None
            }
        }
    }
}
