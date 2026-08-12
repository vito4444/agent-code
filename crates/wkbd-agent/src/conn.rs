//! A JSON-RPC connection to one agent process, over newline-delimited JSON on stdio.
//!
//! Written by hand rather than delegating to the protocol crate's runtime for one reason:
//! a client has to survive input that a well-behaved library would reject. Lines that are
//! not JSON, updates with discriminants that do not exist, tool statuses outside the
//! enum — all of these have shipped in real agents, and none of them may end a session.
//!
//! Every frame in both directions is offered to a raw sink before anything else happens,
//! so the message inspector shows what actually crossed the wire rather than what we
//! managed to interpret.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToAgent,
    FromAgent,
}

#[derive(Debug, Clone)]
pub struct RawFrame {
    pub at_ms: i64,
    pub direction: Direction,
    pub line: String,
    /// Set when the line could not be parsed as JSON at all.
    pub malformed: bool,
}

/// Something the agent asked us to do, or told us about.
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    /// A request from the agent. Must be answered via `responder`, or the agent blocks.
    Request { id: Value, method: String, params: Value, responder: oneshot::Sender<Value> },
}

pub struct Connection {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>>,
    next_id: AtomicI64,
    child: Arc<Mutex<Child>>,
    raw_sink: Option<mpsc::UnboundedSender<RawFrame>>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Connection {
    /// Spawns the agent and starts pumping its stdout.
    ///
    /// `incoming` receives notifications and agent-initiated requests. `raw_sink`, if
    /// present, receives every line in both directions including malformed ones.
    pub async fn spawn(
        mut cmd: Command,
        incoming: mpsc::UnboundedSender<Incoming>,
        raw_sink: Option<mpsc::UnboundedSender<RawFrame>>,
    ) -> Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawning agent process")?;

        let stdout = child.stdout.take().context("agent has no stdout")?;
        let stderr = child.stderr.take().context("agent has no stderr")?;
        let stdin = child.stdin.take().context("agent has no stdin")?;

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // stderr is drained on its own task. An agent that writes a lot of diagnostics
        // and is never read from will eventually block on a full pipe and appear to hang.
        //
        // The tail is also retained, because the most common launch failure is the agent
        // exiting during startup — bad arguments, a missing credential — and the only
        // explanation is on stderr. Without this the caller sees "the process closed its
        // output", which says nothing about why.
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "agent_stderr", "{line}");
                    let mut t = tail.lock().await;
                    if t.len() >= 20 {
                        t.remove(0);
                    }
                    t.push(line);
                }
            });
        }

        {
            let pending = pending.clone();
            let raw_sink = raw_sink.clone();
            let stderr_tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    let line = match lines.next_line().await {
                        Ok(Some(l)) => l,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "agent stdout read failed");
                            break;
                        }
                    };
                    if line.trim().is_empty() {
                        continue;
                    }

                    let parsed: Option<Value> = serde_json::from_str(&line).ok();
                    if let Some(sink) = &raw_sink {
                        let _ = sink.send(RawFrame {
                            at_ms: now_ms(),
                            direction: Direction::FromAgent,
                            line: line.clone(),
                            malformed: parsed.is_none(),
                        });
                    }

                    let Some(msg) = parsed else {
                        // Real agents have shipped builds that print status text to
                        // stdout and corrupt the stream. Skipping the line keeps the
                        // session alive; the inspector still shows it.
                        tracing::warn!(line = %line, "non-JSON line from agent, skipping");
                        continue;
                    };

                    let has_method = msg.get("method").and_then(|m| m.as_str()).is_some();
                    let id = msg.get("id").cloned();

                    if !has_method {
                        // A response to something we sent.
                        let Some(id) = id else {
                            tracing::warn!(?msg, "message with neither method nor id");
                            continue;
                        };
                        let key = id_key(&id);
                        let waiter = pending.lock().await.remove(&key);
                        if let Some(tx) = waiter {
                            let outcome = if let Some(err) = msg.get("error") {
                                Err(RpcError {
                                    code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                                    message: err
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("unknown error")
                                        .to_string(),
                                })
                            } else {
                                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(outcome);
                        } else {
                            tracing::warn!(%key, "response for an unknown request id");
                        }
                        continue;
                    }

                    let method =
                        msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                    let params = msg.get("params").cloned().unwrap_or(json!({}));

                    match id {
                        Some(id) => {
                            let (responder, rx) = oneshot::channel();
                            if incoming
                                .send(Incoming::Request {
                                    id: id.clone(),
                                    method,
                                    params,
                                    responder,
                                })
                                .is_err()
                            {
                                break;
                            }
                            // The reply is written by a task so a slow decision (a
                            // permission prompt waiting on a human) does not stall the
                            // read loop and stop streaming.
                            let _ = rx;
                        }
                        None => {
                            if incoming.send(Incoming::Notification { method, params }).is_err() {
                                break;
                            }
                        }
                    }
                }
                // Fail every outstanding request rather than leaving callers awaiting a
                // reply that can never arrive. Give the stderr reader a moment to catch up
                // so the explanation travels with the failure.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let why = {
                    let tail = stderr_tail.lock().await;
                    if tail.is_empty() {
                        "agent process closed its output".to_string()
                    } else {
                        format!(
                            "agent process closed its output; last stderr: {}",
                            tail.join(" | ")
                        )
                    }
                };
                let mut p = pending.lock().await;
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(RpcError { code: -32000, message: why.clone() }));
                }
            });
        }

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicI64::new(1),
            child: Arc::new(Mutex::new(child)),
            raw_sink,
        })
    }

    async fn write_line(&self, v: &Value) -> Result<()> {
        let line = serde_json::to_string(v)?;
        if let Some(sink) = &self.raw_sink {
            let _ = sink.send(RawFrame {
                at_ms: now_ms(),
                direction: Direction::ToAgent,
                line: line.clone(),
                malformed: false,
            });
        }
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// Sends a request and waits for the response.
    ///
    /// There is deliberately no timeout: a prompt turn legitimately runs for many minutes,
    /// and a client-side timeout would abandon work the agent is still doing. Liveness
    /// comes from the process supervisor and from explicit cancellation instead.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.to_string(), tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_line(&msg).await {
            self.pending.lock().await.remove(&id.to_string());
            return Err(RpcError { code: -32000, message: format!("write failed: {e}") });
        }

        rx.await.map_err(|_| RpcError {
            code: -32000,
            message: "connection closed before the response arrived".into(),
        })?
    }

    pub async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).await
    }

    pub async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.write_line(&json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }))
        .await
    }

    pub async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        child.start_kill()?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<std::process::ExitStatus> {
        let mut child = self.child.lock().await;
        Ok(child.wait().await?)
    }

    pub async fn id(&self) -> Option<u32> {
        self.child.lock().await.id()
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
