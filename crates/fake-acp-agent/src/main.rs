//! A scriptable ACP agent used as a test double.
//!
//! It speaks raw newline-delimited JSON-RPC rather than using the protocol crate's agent
//! abstractions, deliberately: a test double's job includes sending things a well-behaved
//! library would refuse to send — unknown `sessionUpdate` discriminants, chunks with no
//! `messageId`, tool statuses that are not in the enum. If the double could only produce
//! valid messages it could not exercise the client's tolerance for invalid ones.
//!
//! Model and thinking level are read from the environment at startup and are also exposed
//! as session config options. That mirrors the split in the real world: some agents can
//! only be configured at spawn time, some accept `session/set_config_option` at runtime,
//! and the client has to discover which by probing rather than by assuming.

mod scenario;

use anyhow::Result;
use clap::Parser;
use scenario::{Profile, Step};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser, Debug)]
#[command(name = "fake-acp-agent", about = "Scriptable ACP agent for testing clients")]
struct Cli {
    /// Behaviour profile: rich | spartan | resume | stall | alien | flood | crash
    #[arg(long, default_value = "rich", env = "FAKE_ACP_PROFILE")]
    profile: Profile,

    /// Milliseconds between scripted steps. Zero makes runs deterministic and fast.
    #[arg(long, default_value_t = 0, env = "FAKE_ACP_DELAY_MS")]
    delay_ms: u64,

    /// Reported model. Read once at startup, exactly like a real CLI reading argv or env.
    #[arg(long, default_value = "fake-fast", env = "FAKE_ACP_MODEL")]
    model: String,

    /// Whether `session/set_config_option` is honoured at runtime. Off means the client
    /// must fall back to "restart the process to change the model".
    ///
    /// Parsed loosely because these arrive as environment variables and `1` is at least as
    /// common as `true` there. A strict parser turns a typo into "the process exited
    /// during startup", which is a genuinely painful thing to debug through a pipe.
    #[arg(long, env = "FAKE_ACP_LIVE_CONFIG", default_value_t = false,
          num_args = 0..=1, default_missing_value = "true", value_parser = loose_bool)]
    live_config: bool,

    /// Emit a line of non-JSON to stdout before the handshake. Reproduces the class of
    /// bug where an agent prints a banner and corrupts the JSON-RPC stream.
    #[arg(long, env = "FAKE_ACP_POLLUTE_STDOUT", default_value_t = false,
          num_args = 0..=1, default_missing_value = "true", value_parser = loose_bool)]
    pollute_stdout: bool,
}

/// Replaces `{ROOT}` in every string value with the session's working directory.
fn substitute_root(params: &Value, cwd: &str) -> Value {
    match params {
        Value::String(s) => Value::String(s.replace("{ROOT}", cwd)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| substitute_root(i, cwd)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_root(v, cwd)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn loose_bool(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(format!("expected a boolean, got {other:?}")),
    }
}

struct Session {
    profile: Profile,
    config: HashMap<String, Value>,
    cancelled: bool,
    /// The working directory the client gave us at `session/new`. Scenarios substitute it for
    /// `{ROOT}` so they can name paths without knowing where the test put the repository.
    cwd: String,
}

struct State {
    cli: Cli,
    sessions: HashMap<String, Session>,
    next_session: u64,
    /// Ids of permission requests we are waiting on, mapped to the step index to resume.
    /// Requests we are waiting for an answer to, by request id.
    ///
    /// A set rather than one slot. One process serves several sessions — the client's pool keys on
    /// agent plus launch settings, not on session — so two turns can be in flight at once, and a
    /// single slot means one turn's answer clears the other's wait. The stall that produces looks
    /// exactly like a client that did not reply.
    pending: std::collections::HashSet<i64>,
}

fn write_line(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Turns still running. Stdin closing means the client is gone, but a turn spawned just
/// before that must still be allowed to finish and answer, otherwise piping a script into
/// the double silently produces no output at all.
static ACTIVE_TURNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn notify(method: &str, params: Value) {
    write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

fn respond(id: &Value, result: Value) {
    write_line(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn respond_err(id: &Value, code: i64, message: &str) {
    write_line(&json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": code, "message": message }
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.pollute_stdout {
        // Not JSON. A client that assumes every stdout line parses will die here; a
        // client that skips unparseable lines and logs them will survive. Gemini CLI
        // shipped exactly this bug, so it is worth being able to reproduce on demand.
        println!("Loaded cached credentials.");
        let _ = std::io::stdout().flush();
    }

    let state = Arc::new(Mutex::new(State {
        cli,
        sessions: HashMap::new(),
        next_session: 0,
        pending: std::collections::HashSet::new(),
    }));

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fake-acp-agent: unparseable input: {e}");
                continue;
            }
        };

        // A response to something we asked (currently only permission requests).
        if msg.get("method").is_none() && msg.get("id").is_some() {
            let answered = msg.get("id").and_then(|v| v.as_i64());
            let mut s = state.lock().unwrap();
            if let Some(id) = answered {
                s.pending.remove(&id);
            }
            eprintln!("fake-acp-agent: got response {}", msg.get("id").unwrap());
            continue;
        }

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        match method.as_str() {
            "initialize" => {
                let (caps, model) = {
                    let s = state.lock().unwrap();
                    (s.cli.profile.agent_capabilities(), s.cli.model.clone())
                };
                if let Some(id) = id {
                    respond(
                        &id,
                        json!({
                            "protocolVersion": 1,
                            "agentCapabilities": caps,
                            "agentInfo": { "name": "fake-acp-agent", "version": model },
                            "authMethods": []
                        }),
                    );
                }
            }
            "session/new" => {
                let (session_id, config) = {
                    let mut s = state.lock().unwrap();
                    s.next_session += 1;
                    let sid = format!("fake-session-{}", s.next_session);
                    let profile = s.cli.profile;
                    let model = s.cli.model.clone();
                    let mut config = HashMap::new();
                    config.insert("model".to_string(), json!(model));
                    let cwd = params
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .unwrap_or("/")
                        .to_string();
                    s.sessions
                        .insert(sid.clone(), Session { profile, config, cancelled: false, cwd });
                    (sid, profile.config_options())
                };
                if let Some(id) = id {
                    let mut result = json!({ "sessionId": session_id });
                    if let Some(cfg) = config {
                        result["configOptions"] = cfg;
                    }
                    respond(&id, result);
                }
            }
            "session/set_config_option" => {
                let sid = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let cfg_id = params.get("configId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                let (live, options) = {
                    let s = state.lock().unwrap();
                    (s.cli.live_config, s.sessions.get(&sid).map(|x| x.profile.config_options()))
                };
                if !live {
                    if let Some(id) = id {
                        // -32601 method not found is the honest answer from an agent whose
                        // model can only be chosen at spawn time.
                        respond_err(&id, -32601, "this agent only reads configuration at startup");
                    }
                    continue;
                }
                {
                    let mut s = state.lock().unwrap();
                    if let Some(sess) = s.sessions.get_mut(&sid) {
                        sess.config.insert(cfg_id.clone(), value.clone());
                    }
                }
                if let Some(id) = id {
                    let mut opts = options.flatten().unwrap_or(json!([]));
                    if let (Some(arr), Some(v)) = (opts.as_array_mut(), value.as_str()) {
                        for o in arr.iter_mut() {
                            if o.get("id").and_then(|x| x.as_str()) == Some(cfg_id.as_str()) {
                                o["currentValue"] = json!(v);
                            }
                        }
                    }
                    respond(&id, json!({ "configOptions": opts }));
                }
            }
            "session/cancel" => {
                let sid = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mut s = state.lock().unwrap();
                if let Some(sess) = s.sessions.get_mut(&sid) {
                    sess.cancelled = true;
                }
            }
            "session/prompt" => {
                let sid =
                    params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let prompt_text = params
                    .get("prompt")
                    .and_then(|p| p.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();

                let cwd = state
                    .lock()
                    .unwrap()
                    .sessions
                    .get(&sid)
                    .map(|s| s.cwd.clone())
                    .unwrap_or_else(|| "/".to_string());

                let (profile, delay) = {
                    let mut s = state.lock().unwrap();
                    if let Some(sess) = s.sessions.get_mut(&sid) {
                        sess.cancelled = false;
                    }
                    let p = s
                        .sessions
                        .get(&sid)
                        .map(|x| x.profile)
                        .unwrap_or_else(|| s.cli.profile);
                    (p, s.cli.delay_ms)
                };

                let st = state.clone();
                let sid2 = sid.clone();
                ACTIVE_TURNS.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let stop = run_script(st, &sid2, profile, &prompt_text, delay, &cwd).await;
                    if let Some(id) = id {
                        respond(&id, json!({ "stopReason": stop }));
                    }
                    ACTIVE_TURNS.fetch_sub(1, Ordering::SeqCst);
                });
            }
            "" => {}
            other => {
                if let Some(id) = id {
                    respond_err(&id, -32601, &format!("method not found: {other}"));
                }
            }
        }
    }

    // Drain in-flight turns before exiting. Bounded, so a stalled turn cannot keep the
    // double alive forever after its client has gone.
    for _ in 0..1200 {
        if ACTIVE_TURNS.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    Ok(())
}

/// Runs a scripted turn. Returns the stop reason.
async fn run_script(
    state: Arc<Mutex<State>>,
    session_id: &str,
    profile: Profile,
    prompt: &str,
    delay_ms: u64,
    cwd: &str,
) -> &'static str {
    let steps = profile.script(prompt);

    for step in steps {
        if state
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .map(|s| s.cancelled)
            .unwrap_or(false)
        {
            return "cancelled";
        }

        match step {
            Step::Emit(update) => {
                notify(
                    "session/update",
                    json!({ "sessionId": session_id, "update": update }),
                );
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
            Step::AskPermission { tool_call_id, title } => {
                let req_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
                state.lock().unwrap().pending.insert(req_id);
                write_line(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session_id,
                        "toolCall": { "toolCallId": tool_call_id, "title": title },
                        "options": [
                            { "optionId": "allow-once",   "name": "Allow once",           "kind": "allow_once" },
                            { "optionId": "allow-always", "name": "Always allow edits",    "kind": "allow_always" },
                            { "optionId": "reject-once",  "name": "Reject",               "kind": "reject_once" }
                        ]
                    }
                }));
                // Wait for the client to answer, but not forever: a client that never
                // answers must not wedge the double.
                for _ in 0..600 {
                    {
                        let s = state.lock().unwrap();
                        if !s.pending.contains(&req_id) {
                            break;
                        }
                        if s.sessions.get(session_id).map(|x| x.cancelled).unwrap_or(false) {
                            return "cancelled";
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            Step::Sleep(secs) => {
                for _ in 0..(secs * 10) {
                    if state
                        .lock()
                        .unwrap()
                        .sessions
                        .get(session_id)
                        .map(|s| s.cancelled)
                        .unwrap_or(false)
                    {
                        return "cancelled";
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            Step::Die => {
                eprintln!("fake-acp-agent: exiting mid-turn on purpose");
                std::process::exit(7);
            }
            Step::Request { method, params, label } => {
                let params = substitute_root(&params, cwd);
                let req_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
                state.lock().unwrap().pending.insert(req_id);

                let mut full = params;
                full["sessionId"] = json!(session_id);
                write_line(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": method,
                    "params": full
                }));

                // The answer is reported on stderr rather than swallowed. Whether the client
                // allowed or refused is the entire finding for a probing scenario, and a double
                // that hides it would let a broken boundary look like a working one.
                for _ in 0..600 {
                    {
                        let s = state.lock().unwrap();
                        if !s.pending.contains(&req_id) {
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                eprintln!("fake-acp-agent: {label} answered");
            }
        }
    }

    "end_turn"
}
