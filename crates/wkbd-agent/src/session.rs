//! Opening and driving sessions.
//!
//! [`SessionFactory::open`] is the only way to obtain a [`SessionHandle`]. Every path that
//! needs a session — a new chat, resuming an old one, switching to a model that cannot be
//! changed in place, the orchestrator handing work to a worker, replaying a recorded run —
//! goes through it, and each of those is a [`SessionPurpose`] variant.
//!
//! That is not tidiness. User rules and recalled memory are injected at session open, and
//! "the rules mysteriously do not apply in this one situation" is what happens when a new
//! entry point is added and the injection is not. With one constructor and an exhaustive
//! purpose enum, adding an entry point without considering the prelude is a compile error
//! rather than a bug report six weeks later.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::conn::Connection;
use crate::pool::{AgentPool, AgentSpec, LaunchConfig, ProcessKey};
use crate::wire;
use wkbd_proto::{ConfigOptionView, EventPayload, Normalizer, StopReason};

/// Every way a session can come into existence.
///
/// Exhaustive on purpose: a test asserts that the prelude is applied for all of them, so
/// adding a variant forces the question to be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPurpose {
    /// A user starting a conversation.
    NewChat,
    /// Continuing a conversation that already has history, via `session/load`.
    Resume,
    /// The user changed a setting the agent can only read at launch, so this is a fresh
    /// session that inherits a summary of the previous one.
    RelaunchForConfig,
    /// The orchestrator dispatching a task to a worker.
    OrchestratorWorker,
    /// Replaying a recorded run for debugging. Still gets the prelude, because a replay
    /// that behaves differently from the original is not a replay.
    Replay,
}

impl SessionPurpose {
    pub const ALL: &'static [SessionPurpose] = &[
        SessionPurpose::NewChat,
        SessionPurpose::Resume,
        SessionPurpose::RelaunchForConfig,
        SessionPurpose::OrchestratorWorker,
        SessionPurpose::Replay,
    ];
}

/// The text placed in front of the user's own message.
///
/// Rules and memories are separate fields, never one list. They are different kinds of
/// thing: a rule is an instruction the user typed, a memory is evidence we inferred and
/// may have got wrong. Flattened into one list a model weighs them the same way, and an
/// instruction reliably loses to a longer pile of evidence.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prelude {
    /// Verbatim user rules. No confidence annotation, because they are not claims.
    pub rules: Vec<String>,
    /// Recalled memories, each already rendered with its provenance.
    pub memories: Vec<String>,
}

impl Prelude {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.memories.is_empty()
    }

    /// Renders the prelude.
    ///
    /// Rules come first, under their own heading, unmodified. Memories follow under a
    /// heading that says what they are and which one wins on conflict. The ordering and
    /// the labelling are the whole mechanism: there is nothing else stopping a stale
    /// inference from overriding a standing instruction.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.rules.is_empty() {
            out.push_str("## User rules (instructions, follow exactly)\n");
            for r in &self.rules {
                out.push_str("- ");
                out.push_str(r);
                out.push('\n');
            }
            out.push('\n');
        }
        if !self.memories.is_empty() {
            out.push_str(
                "## Recalled context (inferred evidence, may be out of date; \
                 if it conflicts with a user rule above, the rule wins)\n",
            );
            for m in &self.memories {
                out.push_str("- ");
                out.push_str(m);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

/// Supplies the prelude for a session.
///
/// A trait so that `wkbd-agent` does not depend on the memory layer, and so tests can
/// assert injection happened without a database.
pub trait PreludeProvider: Send + Sync {
    fn prelude_for(&self, project_root: &str, purpose: SessionPurpose) -> Prelude;
}

/// A provider that returns nothing. Used where a session genuinely has no project context.
pub struct NoPrelude;

impl PreludeProvider for NoPrelude {
    fn prelude_for(&self, _project_root: &str, _purpose: SessionPurpose) -> Prelude {
        Prelude::default()
    }
}

#[derive(Debug, Clone)]
pub struct SessionOpenRequest {
    pub spec: AgentSpec,
    pub config: LaunchConfig,
    pub project_root: String,
    pub purpose: SessionPurpose,
    /// The agent's own session id, when resuming.
    pub resume_acp_session_id: Option<String>,
    /// Carried across a relaunch so the new session knows what the old one was doing.
    /// There is no in-place model switch; pretending otherwise would be a control that
    /// appears to work and changes nothing.
    pub handoff_summary: Option<String>,
    /// What we tell the agent we can do for it, sent verbatim at `initialize`.
    ///
    /// Supplied by the caller rather than fixed here, because whether to offer client-side file
    /// I/O is a policy decision with a real trade-off on both sides, and the layer that owns the
    /// boundary enforcement is the layer that should decide.
    pub client_capabilities: Value,
}

pub struct SessionHandle {
    pub local_id: String,
    pub acp_session_id: String,
    pub process_key: ProcessKey,
    pub config_options: Vec<ConfigOptionView>,
    pub prelude: Prelude,
    conn: Arc<Connection>,
    normalizer: Arc<Mutex<Normalizer>>,
    spec: AgentSpec,
}

pub struct PromptOutcome {
    pub payloads: Vec<EventPayload>,
    pub stop_reason: StopReason,
}

impl SessionHandle {
    pub fn connection(&self) -> Arc<Connection> {
        self.conn.clone()
    }

    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    /// Begins a turn and returns the events that opening it produced.
    pub async fn begin_turn(&self, prompt: &str) -> Vec<EventPayload> {
        self.normalizer.lock().await.begin_turn(prompt)
    }

    /// Feeds one raw `session/update` payload through normalization.
    pub async fn ingest_update(&self, update: &Value) -> Vec<EventPayload> {
        let live = self.config_options.iter().any(|o| o.live_switchable);
        let raw = wire::map_session_update(update, live);
        self.normalizer.lock().await.push(raw)
    }

    pub async fn end_turn(&self, stop_reason: StopReason) -> Vec<EventPayload> {
        self.normalizer.lock().await.end_turn(stop_reason)
    }

    /// Records that the agent asked for permission, at the position in the transcript
    /// where it asked.
    pub async fn note_permission_request(
        &self,
        request_id: &str,
        params: &Value,
    ) -> (Vec<EventPayload>, Vec<wkbd_proto::PermissionOption>) {
        let options = wire::parse_permission_options(params.get("options"));
        let tool_call_id = params
            .get("toolCall")
            .and_then(|t| t.get("toolCallId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let title = params
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("Permission required")
            .to_string();
        let payloads = self.normalizer.lock().await.note_permission_request(
            request_id,
            tool_call_id,
            title,
            options.clone(),
        );
        (payloads, options)
    }

    pub async fn note_permission_resolved(
        &self,
        request_id: &str,
        option_id: Option<String>,
        auto: bool,
    ) -> Vec<EventPayload> {
        self.normalizer
            .lock()
            .await
            .note_permission_resolved(request_id, option_id, auto)
    }

    pub async fn segmentation_is_best_effort(&self) -> bool {
        self.normalizer.lock().await.segmentation_is_best_effort()
    }

    /// Sends the prompt. Returns when the agent answers with a stop reason.
    ///
    /// The response arriving is the only authoritative end-of-turn signal. Nothing in this
    /// codebase infers the end of a turn from the stream going quiet, because "the model
    /// paused between tool calls" and "the turn is over" look identical from outside and
    /// confusing them is what makes queued messages arrive in the middle of a turn.
    pub async fn send_prompt(&self, text: &str) -> Result<StopReason> {
        let mut blocks = Vec::new();
        let prelude = self.prelude.render();
        if !prelude.is_empty() {
            blocks.push(json!({ "type": "text", "text": prelude }));
        }
        blocks.push(json!({ "type": "text", "text": text }));

        let res = self
            .conn
            .request(
                "session/prompt",
                json!({ "sessionId": self.acp_session_id, "prompt": blocks }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("session/prompt failed: {e}"))?;

        Ok(wire::parse_stop_reason(res.get("stopReason").and_then(|s| s.as_str())))
    }

    pub async fn cancel(&self) -> Result<()> {
        self.conn
            .notify("session/cancel", json!({ "sessionId": self.acp_session_id }))
            .await
    }

    /// Attempts a runtime config change.
    ///
    /// `Ok(None)` means the agent refused, which is the honest signal that the only way to
    /// apply this setting is a new session on a new process. The caller must not silently
    /// swallow it: a selector that appears to change the model without changing anything
    /// is worse than one that says a restart is required.
    pub async fn set_config_option(
        &self,
        option_id: &str,
        value: Value,
    ) -> Result<Option<Vec<ConfigOptionView>>> {
        if !self.spec.is_live(option_id) {
            return Ok(None);
        }
        match self
            .conn
            .request(
                "session/set_config_option",
                json!({ "sessionId": self.acp_session_id, "configId": option_id, "value": value }),
            )
            .await
        {
            Ok(v) => Ok(Some(wire::parse_config_options(v.get("configOptions"), true))),
            Err(e) => {
                tracing::info!(option = option_id, error = %e, "agent refused a runtime config change");
                Ok(None)
            }
        }
    }
}

pub struct SessionFactory {
    pool: Arc<AgentPool>,
    prelude: Arc<dyn PreludeProvider>,
}

impl SessionFactory {
    pub fn new(pool: Arc<AgentPool>, prelude: Arc<dyn PreludeProvider>) -> Self {
        Self { pool, prelude }
    }

    /// The single entry point for creating sessions.
    pub async fn open(
        &self,
        req: SessionOpenRequest,
        build_command: impl FnMut(&AgentSpec, &LaunchConfig) -> tokio::process::Command,
    ) -> Result<SessionHandle> {
        let conn = self.pool.acquire(&req.spec, &req.config, build_command).await?;
        let process_key = AgentPool::key_for(&req.spec, &req.config);

        conn.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": req.client_capabilities,
                "clientInfo": { "name": "wkbd", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("initialize failed: {e}"))?;

        // Resume where possible so history is not lost, but fall back to a new session
        // rather than failing: an agent that cannot load a session should still be usable.
        let (acp_session_id, raw_options) = match &req.resume_acp_session_id {
            Some(prior) => {
                match conn
                    .request(
                        "session/load",
                        json!({ "sessionId": prior, "cwd": req.project_root, "mcpServers": [] }),
                    )
                    .await
                {
                    Ok(v) => (prior.clone(), v.get("configOptions").cloned()),
                    Err(e) => {
                        tracing::warn!(error = %e, "session/load failed, opening a fresh session");
                        new_session(&conn, &req).await?
                    }
                }
            }
            None => new_session(&conn, &req).await?,
        };

        let live_ids: Vec<String> = req.spec.live_config_ids.clone();
        let mut config_options = wire::parse_config_options(raw_options.as_ref(), false);
        for o in config_options.iter_mut() {
            o.live_switchable = live_ids.iter().any(|i| *i == o.id);
        }

        // The prelude is fetched here, for every purpose, with no branch that can skip it.
        let mut prelude = self.prelude.prelude_for(&req.project_root, req.purpose);
        if let Some(summary) = &req.handoff_summary {
            prelude.memories.push(format!(
                "Continuing from an earlier session that had to be restarted to apply a \
                 setting. Summary of what happened so far: {summary}"
            ));
        }

        Ok(SessionHandle {
            local_id: uuid::Uuid::new_v4().to_string(),
            acp_session_id,
            process_key,
            config_options,
            prelude,
            conn,
            normalizer: Arc::new(Mutex::new(Normalizer::new())),
            spec: req.spec,
        })
    }

    pub fn pool(&self) -> Arc<AgentPool> {
        self.pool.clone()
    }
}

async fn new_session(
    conn: &Arc<Connection>,
    req: &SessionOpenRequest,
) -> Result<(String, Option<Value>)> {
    let v = conn
        .request(
            "session/new",
            json!({ "cwd": req.project_root, "mcpServers": [] }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("session/new failed: {e}"))?;
    let id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .context("session/new returned no sessionId")?
        .to_string();
    Ok((id, v.get("configOptions").cloned()))
}
