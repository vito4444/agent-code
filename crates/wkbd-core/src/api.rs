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

use crate::mentions;
use crate::runner::{run_turn, AskUser, PermissionWaiter, TurnContext};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/agents", get(list_agents))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/prompt", post(prompt))
        .route("/api/sessions/{id}/paths", get(session_paths))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/permission", post(answer_permission))
        .route("/api/sessions/{id}/config", post(set_config))
        .route("/api/terminals/{id}", get(terminal_output))
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/merge", post(merge_run))
        .route("/api/runs/{id}/abandon", post(abandon_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/tasks/{task}/diff", get(task_diff))
        .route("/api/runs/{id}/candidate/diff", get(candidate_diff))
        .route("/api/proposals", get(list_proposals))
        .route("/api/proposals/{id}", get(review_proposal))
        .route("/api/proposals/{id}/approve", post(approve_proposal))
        .route("/api/proposals/{id}/reject", post(reject_proposal))
        .route("/api/rules", get(list_rules).post(create_rule))
        .route("/api/rules/{id}", delete(delete_rule))
        .route("/api/raw", get(raw_frames))
        .route("/api/stream", get(stream))
        .with_state(state)
}

/// Adds the built interface to a router.
///
/// The daemon serves the interface as well as the API, from one origin. During development Vite
/// serves the assets and proxies the API here, and in a packaged build this serves both — the same
/// request URLs either way, so the code that runs in a browser and the code that runs in the desktop
/// shell are the same code.
///
/// The alternative is what the shell tried first: assets from the webview's own protocol, API over
/// HTTP. That gives the two halves different origins, and then every cross-origin question — cookies,
/// WebSocket upgrade, CSP — has to be answered a second time, for the platform that is hardest to
/// test.
///
/// Unknown paths fall back to `index.html` rather than 404ing, because the interface routes
/// client-side: a reload on any screen other than the first would otherwise land on a 404.
pub fn with_interface(router: Router, dist: &std::path::Path) -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let index = dist.join("index.html");
    router.fallback_service(ServeDir::new(dist).fallback(ServeFile::new(index)))
}

/// Where the built interface is, if it is anywhere.
///
/// Beside the binary first, because that is where a packaged build puts it. `ui/dist` relative to the
/// working directory second, for running from a checkout. Returning `None` rather than a guess: the
/// API has to keep working when the interface was never built, and a router with a fallback pointing
/// at a directory that does not exist answers every unknown path with a confusing 500 instead of a
/// 404.
pub fn find_interface() -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ui"));
            candidates.push(dir.join("../ui/dist"));
        }
    }
    candidates.push(std::path::PathBuf::from("ui/dist"));
    candidates.into_iter().find(|p| p.join("index.html").is_file())
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "read_only": state.store.is_read_only(),
        "degraded": state.degraded,
        "agents": state.agents.len(),
        // So a caller that started a daemon can check it is talking to the one it started. The
        // desktop shell binds a fixed port, and a stale daemon still holding that port means the new
        // one fails to bind and exits while the shell — which only asked whether *something* answers
        // — adopts the stranger. Every symptom of that is a mystery: settings that do not apply,
        // agents that are not there, a version mismatch with no version in sight.
        "pid": std::process::id(),
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
    /// The project a rule written "for this project" should attach to.
    ///
    /// A worker's `project_root` is a worktree that will be deleted when the run ends, so
    /// offering it as a rule's scope offers to write a rule nothing will ever read again.
    memory_scope: String,
    title: Option<String>,
    /// What this agent said it can be handed in a prompt. Per session rather than per agent
    /// because it is negotiated at `initialize`, and the same binary launched with a different
    /// model can answer differently.
    prompt_capabilities: wkbd_proto::PromptCapabilities,
}

impl SessionSummary {
    fn of(s: &crate::state::LiveSession) -> Self {
        Self {
            id: s.handle.local_id.clone(),
            agent_id: s.agent_id.clone(),
            agent_display_name: s.agent_display_name.clone(),
            project_root: s.project_root.clone(),
            memory_scope: s.memory_scope.clone(),
            title: s.title.clone(),
            prompt_capabilities: s.handle.prompt_capabilities(),
        }
    }
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let out: Vec<SessionSummary> = sessions.values().map(|s| SessionSummary::of(s)).collect();
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
        .open_session(
            &body.agent_id,
            &body.project_root,
            &body.project_root,
            SessionPurpose::NewChat,
        )
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(SessionSummary::of(&session)))
}

#[derive(Deserialize)]
struct PromptBody {
    text: String,
    /// Project-relative paths the user attached, in the order they should reach the agent.
    ///
    /// Relative, never absolute: an absolute path from the client is a path the client chose,
    /// and the guard exists so that no client choice decides what gets opened.
    #[serde(default)]
    mentions: Vec<String>,
}

async fn prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PromptBody>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state.session(&id).await.ok_or_else(ApiError::not_found)?;

    // Before the session is marked busy, because this can fail and a failure here must not
    // leave a session that never started a turn unable to start one.
    let attached = resolve_mentions(&session, &body)?;

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
        let publisher = state2.clone();
        let ctx = TurnContext {
            store: state2.store.clone(),
            publish: Arc::new(move |events: &[wkbd_proto::Event]| publisher.publish(events)),
            session_local_id: session2.handle.local_id.clone(),
            handle: session2.handle.clone(),
            permissions: state2.permissions.clone(),
            project_root: session2.project_root.clone(),
            ask_user: Arc::new(PendingPrompt::new(state2.clone())),
            guard: session2.guard.clone(),
            offer_client_fs: session2.offer_client_fs,
        };
        let mut inbox = session2.inbox.lock().await;
        if let Err(e) = run_turn(&ctx, &text, attached, &mut inbox).await {
            tracing::error!(error = %e, "turn failed");
        }
        *session2.busy.lock().await = false;
    });

    Ok(StatusCode::ACCEPTED)
}

/// Resolves the attachments for one prompt into wire blocks and transcript records.
///
/// The union of what the composer sent and what the text contains, in the order the paths appear.
/// Two sources because they can disagree: the composer knows what was picked from the completion
/// list, and the text is what the user actually left in the box after editing it. Trusting only
/// the list attaches a file the user deleted from the message; trusting only the text loses a
/// mention whose path has a space in it. Taking both, deduplicated, is wrong in neither direction.
fn resolve_mentions(
    session: &Arc<crate::state::LiveSession>,
    body: &PromptBody,
) -> Result<crate::runner::Attached, ApiError> {
    let mut wanted: Vec<String> = Vec::new();
    for path in body.mentions.iter().cloned().chain(mentions::scan_text(&body.text)) {
        if !wanted.contains(&path) {
            wanted.push(path);
        }
    }
    if wanted.is_empty() {
        return Ok(crate::runner::Attached::default());
    }

    let Some(guard) = &session.guard else {
        return Err(ApiError::bad_request(
            "this session has no filesystem boundary, so nothing can be attached to it",
        ));
    };

    // Hand-typed `@words` that are not paths are dropped rather than refused: someone writing
    // "ask @alice about this" has not made a mistake, and failing their prompt over it would be
    // maddening. A path that came from the completion list is a different matter — the user
    // chose a real file, so if it cannot be read now, saying so beats sending a prompt that
    // refers to a file the agent never got.
    let typed_only: Vec<String> =
        mentions::scan_text(&body.text).into_iter().filter(|p| !body.mentions.contains(p)).collect();

    let root = mentions::root_of(&session.project_root);
    let caps = session.handle.prompt_capabilities();

    let mut blocks = Vec::new();
    let mut records = Vec::new();
    for path in wanted {
        match mentions::resolve(guard, &root, std::slice::from_ref(&path), caps) {
            Ok(mut r) => {
                blocks.append(&mut r.blocks);
                records.append(&mut r.attachments);
            }
            Err(e) if typed_only.contains(&path) => {
                tracing::debug!(%path, error = %e, "an @word in the message is not a file");
            }
            Err(e) => return Err(ApiError::bad_request(&e.to_string())),
        }
    }

    Ok(crate::runner::Attached { blocks, records })
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    q: String,
}

/// Files under a session's project root, for the composer's `@` completion.
///
/// Scoped to a session rather than taking a root as a parameter, so that the set of directories
/// this endpoint will ever enumerate is exactly the set the user already opened a session on.
/// A `?root=` parameter would make the daemon a general-purpose filesystem browser for anything
/// that can reach the port.
async fn session_paths(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let session = state.session(&id).await.ok_or_else(ApiError::not_found)?;
    let root = mentions::root_of(&session.project_root);
    let entries = tokio::task::spawn_blocking(move || mentions::search(&root, &q.q, 40))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(entries))
}

/// The queue of things the system wants to tell itself.
///
/// Nothing here is in effect. That is the point: the system's own instructions are the highest-value
/// target for an injection, and a run's transcript contains text from tools, files and possibly a
/// hostile repository. So they arrive as proposals with their evidence and wait.
async fn list_proposals(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let pending = wkbd_evolve::proposals::pending(&state.store)
        .await
        .map_err(ApiError::internal)?;
    let items: Vec<serde_json::Value> = pending
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "kind": p.kind.as_str(),
                "scope": p.scope,
                "risk": p.risk.as_str(),
                "created_ms": p.created_ms,
                // Not the body. A list is skim-read, and the body is the part that has to be read
                // carefully with the invisible characters already stripped — which is what the
                // review endpoint does. Putting raw bodies in a list invites approving from the list.
                "requires_distinct_confirmation": p.requires_distinct_confirmation(),
            })
        })
        .collect();
    Ok(Json(json!({ "proposals": items })))
}

/// One proposal, prepared for a human to read.
///
/// The body comes back with invisible characters removed and a summary of what was removed, because
/// a reviewer cannot consent to text they cannot see. The content hash comes back too and has to be
/// handed to the approval: an edit that lands between rendering and clicking invalidates the click
/// rather than being carried along by it.
async fn review_proposal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let p = wkbd_evolve::proposals::present_for_review(&state.store, &id)
        .await
        .map_err(|e| ApiError { status: StatusCode::NOT_FOUND, message: e.to_string() })?;
    Ok(Json(json!({
        "id": p.id,
        "kind": p.kind.as_str(),
        "scope": p.scope,
        "risk": p.risk.as_str(),
        "content_hash": p.content_hash,
        // What it is asking for, in sentences.
        //
        // The stored body is the serialised payload, and showing that alone made the reviewer dig
        // one sentence out of a line of JSON — which is the shape of an approval given to something
        // nobody read. This is a rendering of the same bytes and never a substitute for them:
        // `body_for_human` is still sent, the interface keeps it one click away, and the hash is
        // still over the bytes rather than over this.
        "changes": describe_payload(&p.body_for_human),
        "body_for_human": p.body_for_human,
        "hidden_summary": p.hidden_summary,
        // Each removal located precisely, because "we removed 3 invisible characters" is not
        // reviewable: a reviewer deciding whether the removal changed the meaning needs to know
        // where they were.
        "hidden": p.hidden.iter().map(|r| json!({
            "codepoint": format!("U+{:04X}", r.codepoint),
            "line": r.line,
            "column": r.column,
            "kind": format!("{:?}", r.kind),
        })).collect::<Vec<_>>(),
        "requires_distinct_confirmation": p.requires_distinct_confirmation,
        "confirmation_phrase": p.confirmation_phrase,
        "evidence": {
            "supporting_runs": p.evidence.supporting_runs,
            "verified_signals": p.evidence.verified_signals,
            "note": p.evidence.note,
        },
    })))
}

/// Turns a serialised proposal payload into lines a person can read.
///
/// Falls back to the raw text when it cannot be parsed rather than inventing a summary. A proposal
/// whose payload this does not recognise is exactly the one where a confident-looking summary would
/// be most misleading, and the exact bytes are on screen either way.
fn describe_payload(body: &str) -> Vec<String> {
    use wkbd_evolve::proposals::{PolicySetting, ProposalPayload};
    let Ok(payload) = serde_json::from_str::<ProposalPayload>(body) else {
        return vec![body.to_string()];
    };
    match payload {
        ProposalPayload::Playbook { deltas, .. } => deltas
            .iter()
            .map(|d| match d {
                wkbd_evolve::PlaybookDelta::Add { body, .. } => {
                    format!("Add to the playbook: {body}")
                }
                wkbd_evolve::PlaybookDelta::Update { id, set_body, helpful, harmful, .. } => {
                    match set_body {
                        Some(text) => format!("Reword note {}: {text}", &id[..id.len().min(8)]),
                        // Counters rather than text. Worth spelling out, because "update" on its own
                        // reads as an edit and this one changes nothing anybody wrote.
                        None => format!(
                            "Credit note {} with {helpful} helpful and {harmful} harmful",
                            &id[..id.len().min(8)]
                        ),
                    }
                }
                wkbd_evolve::PlaybookDelta::Deprecate { id, reason } => format!(
                    "Retire note {}: {reason}",
                    &id[..id.len().min(8)]
                ),
            })
            .collect(),
        // One entry, because a workflow is one change. Returning the steps as separate entries put
        // each of them in the queue's own bullet list *and* gave it a number, so every step read
        // "• 1. write *.rs". The order matters and is carried by the prose instead.
        ProposalPayload::Workflow { name, steps, .. } => {
            vec![format!(
                "Remember a procedure for {name:?}: {}",
                steps.join(", then ")
            )]
        }
        ProposalPayload::Policy { setting } => vec![match setting {
            PolicySetting::RoutingCostWeight { value } => {
                format!("Set the router's price penalty to {value}")
            }
            PolicySetting::RoutingQualityTarget { value } => {
                format!("Set the router's quality floor to {value}")
            }
            PolicySetting::ApprovalsPerDay { value } => format!(
                "Allow {value} proposals a day. This changes the machinery that asks for approval."
            ),
        }],
    }
}

#[derive(Deserialize)]
struct Approval {
    /// The hash the reviewer was shown. Required, not optional: an approval that does not say what
    /// it approved cannot be checked against what is there now.
    content_hash: String,
    /// The phrase, for proposals that ask for a different gesture than a click.
    typed: Option<String>,
}

async fn approve_proposal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Approval>,
) -> Result<impl IntoResponse, ApiError> {
    let confirmation = match body.typed {
        Some(typed) => wkbd_evolve::proposals::Confirmation::Distinct { typed },
        None => wkbd_evolve::proposals::Confirmation::Standard,
    };
    let approved =
        wkbd_evolve::proposals::approve(&state.store, &id, &body.content_hash, confirmation)
            .await
            // A stale hash and a wrong phrase are both refusals of this request rather than server
            // faults, and the message says which.
            .map_err(|e| ApiError::conflict(&e.to_string()))?;

    // Applied in the same request that approved it.
    //
    // Two steps, one act. Recording the approval and never running it is what happened first, and
    // from outside it is indistinguishable from the loop working: the queue empties, the proposal
    // says approved, and nothing changed. Worse than not having the queue, because it looks closed.
    //
    // Apply re-checks the hash itself before interpreting the body, and voids the approval rather
    // than proceeding if the bytes moved in between — so this is not the check, it is the caller.
    let applied = wkbd_evolve::proposals::apply(&state.store, &id)
        .await
        .map_err(|e| ApiError::conflict(&e.to_string()))?;

    Ok(Json(json!({
        "id": approved.id,
        "status": "applied",
        "applied": format!("{applied:?}"),
    })))
}

async fn reject_proposal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    wkbd_evolve::proposals::reject(&state.store, &id)
        .await
        .map_err(|e| ApiError::conflict(&e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

/// How many files one request will read either side of.
///
/// A cap rather than a judgement about what is reviewable: the alternative is a request that reads a
/// whole repository into the daemon because a task touched a generated directory.
const MAX_REVIEW_FILES: usize = 200;

/// What one task changed.
///
/// From its own starting commit to its result, which for a dependent task means the diff excludes
/// everything its dependencies produced — it starts from a merge of them. That is the diff worth
/// reviewing: the question at a task card is what *this* task did, and a diff against the run base
/// would answer a different one.
async fn task_diff(
    State(state): State<Arc<AppState>>,
    Path((id, task)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let (_, project_root) = run_row(&state, &id).await?;
    let (from, to) = task_range(&state, &id, &task).await?;
    let set = wkbd_vcs::changes_between(
        std::path::Path::new(&project_root),
        &from,
        &to,
        MAX_REVIEW_FILES,
    )
    .map_err(ApiError::internal)?;
    Ok(Json(set))
}

/// What the whole run would add to the branch.
async fn candidate_diff(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (_, project_root) = run_row(&state, &id).await?;
    let (base, candidate) = candidate_range(&state, &id).await?;
    let set = wkbd_vcs::changes_between(
        std::path::Path::new(&project_root),
        &base,
        &candidate,
        MAX_REVIEW_FILES,
    )
    .map_err(ApiError::internal)?;
    Ok(Json(set))
}

async fn run_row(state: &Arc<AppState>, run_id: &str) -> Result<(String, String), ApiError> {
    let id = run_id.to_string();
    state
        .store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT goal, project_root FROM runs WHERE id = ?1",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?)
        })
        .await
        .map_err(|_| ApiError { status: StatusCode::NOT_FOUND, message: "no such run".into() })
}

/// The two commits a task's diff is taken between, read back out of the run's own events.
///
/// From the log rather than from a table, because the log is the record either way and a second copy
/// of the same fact is a second copy that can disagree with it.
async fn task_range(
    state: &Arc<AppState>,
    run_id: &str,
    task_id: &str,
) -> Result<(String, String), ApiError> {
    let stream = wkbd_proto::run_stream_id(run_id);
    let (events, _) = state
        .store
        .read_since(Some(&stream), 0, 10_000)
        .map_err(ApiError::internal)?;

    let mut from = None;
    for event in &events {
        if let wkbd_proto::EventPayload::Run { run } = &event.payload {
            if let wkbd_proto::RunEvent::TaskWorkspaceReady { task_id: t, start_commit, .. } = run {
                if t == task_id {
                    from = Some(start_commit.clone());
                }
            }
        }
    }

    let from = from.ok_or_else(|| {
        ApiError { status: StatusCode::NOT_FOUND, message: "that task has no workspace".into() }
    })?;

    // The branch tip, rather than a commit recorded in an event. A task's result is a commit made in
    // its worktree, and the branch is what points at it — reading the ref means this answers
    // correctly for a task that is still working, where a recorded final commit does not exist yet.
    let engine = state
        .runs()
        .ok_or_else(|| ApiError::not_implemented("runs are not configured"))?;
    let to = engine
        .task_tip(run_id, task_id)
        .await
        .map_err(|e| ApiError { status: StatusCode::NOT_FOUND, message: e.to_string() })?;
    Ok((from, to))
}

async fn candidate_range(
    state: &Arc<AppState>,
    run_id: &str,
) -> Result<(String, String), ApiError> {
    let stream = wkbd_proto::run_stream_id(run_id);
    let (events, _) = state
        .store
        .read_since(Some(&stream), 0, 10_000)
        .map_err(ApiError::internal)?;

    let mut base = None;
    let mut candidate = None;
    for event in &events {
        if let wkbd_proto::EventPayload::Run { run } = &event.payload {
            match run {
                wkbd_proto::RunEvent::Started { base_commit, .. } => {
                    base = Some(base_commit.clone())
                }
                wkbd_proto::RunEvent::AwaitingMerge { commit, .. } => {
                    candidate = Some(commit.clone())
                }
                _ => {}
            }
        }
    }

    match (base, candidate) {
        (Some(b), Some(c)) => Ok((b, c)),
        _ => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "this run has no candidate to review".into(),
        }),
    }
}

/// Stops a run.
///
/// Distinct from abandoning one. Cancelling stops work that is still happening; abandoning declines
/// to merge a candidate that already exists. Offering only the second would leave the button that
/// says "cancel" doing nothing to the agents currently running, which is worse than not offering it.
async fn cancel_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let engine = state
        .runs()
        .ok_or_else(|| ApiError::not_implemented("runs are not configured"))?;
    engine.cancel(&id).await.map_err(ApiError::internal)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct CreateRun {
    goal: String,
    project_root: String,
}

async fn create_run(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRun>,
) -> Result<impl IntoResponse, ApiError> {
    let engine = state
        .runs()
        .ok_or_else(|| ApiError::not_implemented("no worker agent is configured for runs"))?;

    // Rejected here rather than discovered three steps in. A run against a directory that is not a
    // repository fails at the first worktree, after the planner has already been paid for.
    let root = std::path::Path::new(&body.project_root);
    if !root.join(".git").exists() {
        return Err(ApiError::bad_request(
            "a run needs a git repository: parallel isolation is worktrees, and there is no .git              here",
        ));
    }
    if body.goal.trim().is_empty() {
        return Err(ApiError::bad_request("a run needs a goal"));
    }

    let id = engine
        .start(&body.goal, &body.project_root)
        .await
        .map_err(ApiError::internal)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id }))))
}

async fn list_runs(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let rows = state
        .store
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, goal, project_root, status, created_ms, updated_ms
                 FROM runs ORDER BY created_ms DESC LIMIT 200",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "goal": r.get::<_, String>(1)?,
                    "project_root": r.get::<_, String>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "created_ms": r.get::<_, i64>(4)?,
                    "updated_ms": r.get::<_, i64>(5)?,
                }))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "runs": rows })))
}

/// A run plus its events, so a client that joins late can render it without replaying the whole log.
async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let stream = wkbd_proto::run_stream_id(&id);
    let (events, _) = state
        .store
        .read_since(Some(&stream), 0, 5000)
        .map_err(ApiError::internal)?;
    if events.is_empty() {
        return Err(ApiError { status: StatusCode::NOT_FOUND, message: "no such run".into() });
    }
    Ok(Json(json!({ "id": id, "events": events })))
}

async fn merge_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let engine = state
        .runs()
        .ok_or_else(|| ApiError::not_implemented("runs are not configured"))?;
    // The only path by which anything a run produced reaches the user's branch, and it exists only
    // because a person asked. Verification proves the named tests pass; it does not prove the change
    // was wanted, and the measured rate at which models exploit a writable test suite is high enough
    // that "the tests pass" cannot be the last word.
    let commit = engine.merge(&id).await.map_err(|e| ApiError::conflict(&e.to_string()))?;
    Ok(Json(json!({ "commit": commit })))
}

async fn abandon_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let engine = state
        .runs()
        .ok_or_else(|| ApiError::not_implemented("runs are not configured"))?;
    engine.abandon(&id).await.map_err(ApiError::internal)?;
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
    Err::<Json<serde_json::Value>, _>(ApiError::not_implemented(
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
    /// Highest `seq` the caller already has. Omitted means "the tail, whatever it is".
    since: Option<u64>,
}

/// Frames, either the tail or everything after a cursor.
///
/// The cursor exists because this screen polls: without one it re-sent, re-parsed and re-rendered
/// the whole visible log every second whether or not anything had changed — and this is the screen
/// somebody opens when an agent is already misbehaving, which is the worst moment for the interface
/// to be adding work of its own.
async fn raw_frames(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RawQuery>,
) -> impl IntoResponse {
    let log = state.raw.lock().await;
    match q.since {
        // Ordered by construction, so a scan from the back stops at the first one already seen.
        // The alternative — a binary search — would be wrong the moment the ring drops entries,
        // because then the buffer's first seq is not 1.
        Some(since) => {
            let start = log.partition_point(|f| f.seq <= since);
            Json(log[start..].to_vec())
        }
        None => {
            let limit = q.limit.unwrap_or(500).min(5_000);
            let start = log.len().saturating_sub(limit);
            Json(log[start..].to_vec())
        }
    }
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
    async fn register(&self, request_id: &str) -> PermissionWaiter {
        let rx = self.state.register_permission_wait(request_id).await;
        // Ten minutes, then refuse. Long enough for someone to come back from a meeting, short
        // enough that an abandoned prompt does not hold an agent open indefinitely.
        PermissionWaiter::new(rx, std::time::Duration::from_secs(600))
    }

    async fn cancel(&self, request_id: &str) -> bool {
        self.state.clear_permission_wait(request_id).await
    }
}
