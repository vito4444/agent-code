//! Scripted agent behaviours.
//!
//! Every scenario here exists because it is something a naive test double does *not* do.
//! A double that emits one thought, one tool call and one answer per turn will pass a
//! broken segmenter, a broken permission flow and a broken context-usage widget without
//! complaint. These scenarios are the counterexamples.

use serde_json::{json, Value};

/// One scripted step. `Emit` sends a `session/update` notification; the others model
/// things a real agent does that are easy to forget.
#[derive(Debug, Clone)]
pub enum Step {
    /// Send a `session/update` with this `update` object, verbatim.
    Emit(Value),
    /// Ask the client for permission and wait for the answer before continuing.
    AskPermission { tool_call_id: String, title: String },
    /// Pause, so cancellation and queueing have a window to happen in.
    Sleep(u64),
    /// Terminate the process mid-turn without answering the prompt.
    Die,
    /// Send a request to the client and wait for its answer.
    ///
    /// `{ROOT}` in any string value is replaced with the session's working directory, so a
    /// scenario can name paths relative to wherever the test put the repository.
    Request { method: String, params: Value, label: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Everything the protocol allows: messageId on every chunk, several thoughts per
    /// turn, a diff, a terminal, a permission request, config options, usage reporting.
    Rich,
    /// The realistic floor: no messageId, no configOptions, no usage_update. This is the
    /// profile most ACP agents actually match today, so it must be the one the UI is
    /// most confident in.
    Spartan,
    /// Returns to an earlier messageId after a tool call, exercising upsert semantics.
    Resume,
    /// Opens a tool call and then stalls, so the client can cancel it.
    Stall,
    /// Emits a `sessionUpdate` discriminant that does not exist in the schema, plus a
    /// chunk whose content shape we do not model. Neither may crash the client.
    Alien,
    /// High-rate chunk flood, for backpressure and UI batching.
    Flood,
    /// Exits mid-turn, leaving the prompt request unanswered.
    Crash,
    /// Does the work a task asks for by writing the file named in the prompt.
    ///
    /// Reads the prompt for `WRITE <path> <<<content>>>` directives and performs them through the
    /// client, so an orchestrated run really produces file changes, real commits and a real merge.
    Worker,
    /// Exercises client-side file access, including four attempts to leave the workspace.
    ///
    /// The escapes are the point. A path guard with passing unit tests says the guard is correct;
    /// it says nothing about whether the daemon wired it into the protocol path, and "the
    /// capability was declared but the check was skipped" looks exactly like success from
    /// outside.
    FsProbe,
}

impl std::str::FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "rich" => Profile::Rich,
            "spartan" => Profile::Spartan,
            "resume" => Profile::Resume,
            "stall" => Profile::Stall,
            "alien" => Profile::Alien,
            "flood" => Profile::Flood,
            "crash" => Profile::Crash,
            "fsprobe" => Profile::FsProbe,
            "worker" => Profile::Worker,
            other => return Err(format!("unknown profile: {other}")),
        })
    }
}

impl Profile {
    /// Capabilities announced at `initialize`.
    pub fn agent_capabilities(self) -> Value {
        match self {
            Profile::Spartan => json!({ "loadSession": false }),
            _ => json!({
                "loadSession": true,
                "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true }
            }),
        }
    }

    /// Config options returned from `session/new`.
    ///
    /// Spartan returns none at all, which is the case the UI has to handle by drawing no
    /// model selector rather than an empty one.
    pub fn config_options(self) -> Option<Value> {
        match self {
            Profile::Spartan | Profile::Crash => None,
            _ => Some(json!([
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "fake-fast",
                    "options": [
                        { "value": "fake-fast", "name": "Fake Fast", "description": "The quick one" },
                        { "value": "fake-deep", "name": "Fake Deep", "description": "The slow one" }
                    ]
                },
                {
                    "id": "thought_level",
                    "name": "Thinking",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "medium",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" }
                    ]
                },
                {
                    // No category at all. The UI must still render this as a plain
                    // toggle rather than dropping it or guessing what it means.
                    "id": "verbose",
                    "name": "Verbose logging",
                    "type": "boolean",
                    "currentValue": false
                },
                {
                    // A vendor-private category. Unknown categories must degrade, and
                    // the `_` prefix is explicitly reserved for custom use by the spec.
                    "id": "_vendor_mode",
                    "name": "Vendor mode",
                    "category": "_vendor_private",
                    "type": "select",
                    "currentValue": "a",
                    "options": [{ "value": "a", "name": "A" }, { "value": "b", "name": "B" }]
                }
            ])),
        }
    }

    /// `turn` is 1 for the first prompt in a session. Only the rich profile uses it, to make
    /// its reported usage grow the way a real context window does — a fixed figure per turn
    /// makes every difference across a turn zero, which is indistinguishable from an agent
    /// that reports nothing.
    pub fn script(self, prompt: &str, turn: u64) -> Vec<Step> {
        match self {
            Profile::Rich => rich(prompt, turn),
            Profile::Spartan => spartan(prompt),
            Profile::Resume => resume(),
            Profile::Stall => stall(),
            Profile::Alien => alien(),
            Profile::Flood => flood(),
            Profile::Crash => vec![
                Step::Emit(thought(Some("m1"), "starting something I will not finish")),
                Step::Sleep(30),
                Step::Die,
            ],
            Profile::FsProbe => fs_probe(),
            // Built from the prompt rather than fixed, because the point of this profile is that
            // the run's own graph decides what happens.
            Profile::Worker => worker(prompt),
        }
    }
}

/// Reads and writes through the client, then tries to leave the workspace four ways.
///
/// Each escape is a published defect shape rather than an invented one: an absolute path outside
/// the roots, a symlink inside the roots pointing out, a symlink as an intermediate component, and
/// a sibling directory whose name begins with an allowed root's name.
fn fs_probe() -> Vec<Step> {
    vec![
        Step::Emit(thought(Some("m1"), "reading a file the honest way")),
        Step::Request {
            method: "fs/read_text_file".into(),
            params: json!({ "path": "{ROOT}/inside.txt" }),
            label: "read-inside".into(),
        },
        Step::Request {
            method: "fs/write_text_file".into(),
            params: json!({
                "path": "{ROOT}/written-by-agent.txt",
                "content": "written through the client\n"
            }),
            label: "write-inside".into(),
        },
        // The protocol requires the client to create a file that does not exist, so this is the
        // normal path and not an edge case — and it is the condition under which the published
        // defects in this area triggered.
        Step::Request {
            method: "fs/write_text_file".into(),
            params: json!({
                "path": "{ROOT}/nested/new.txt",
                "content": "created on demand\n"
            }),
            label: "write-missing-parent".into(),
        },
        Step::Emit(thought(Some("m2"), "now trying to get out")),
        Step::Request {
            method: "fs/read_text_file".into(),
            params: json!({ "path": "/etc/passwd" }),
            label: "escape-absolute".into(),
        },
        Step::Request {
            method: "fs/read_text_file".into(),
            params: json!({ "path": "{ROOT}/escape-link" }),
            label: "escape-symlink".into(),
        },
        Step::Request {
            method: "fs/read_text_file".into(),
            params: json!({ "path": "{ROOT}/via/link/secret.txt" }),
            label: "escape-symlink-component".into(),
        },
        Step::Request {
            method: "fs/read_text_file".into(),
            params: json!({ "path": "{ROOT}_evil/secret.txt" }),
            label: "escape-prefix".into(),
        },
        Step::Request {
            method: "fs/write_text_file".into(),
            params: json!({ "path": "/tmp/wkbd-should-not-exist", "content": "pwned\n" }),
            label: "escape-write".into(),
        },
        Step::Emit(message(Some("m3"), "finished probing")),
    ]
}

/// Performs the `WRITE <path> <<<content>>>` directives in a task body.
///
/// Deliberately mechanical. What the end-to-end test needs to establish is that the orchestrator
/// creates real isolation, transports real work across dependency edges and produces real commits;
/// none of that is affected by whether the agent doing the work was clever. An agent whose output
/// varies would make the test unable to distinguish "the orchestrator is wrong" from "the agent
/// chose differently this time".
fn worker(prompt: &str) -> Vec<Step> {
    let mut steps = vec![Step::Emit(thought(Some("m1"), "reading the task"))];
    let mut wrote = 0;

    for directive in prompt.split("WRITE ").skip(1) {
        let Some((path, rest)) = directive.split_once(" <<<") else { continue };
        let Some((content, _)) = rest.split_once(">>>") else { continue };
        let path = path.trim();
        // Relative to the worktree, so the same task body works in whichever worktree it lands in.
        // An absolute path would make the graph depend on where the test put the repository.
        steps.push(Step::Request {
            method: "fs/write_text_file".into(),
            params: json!({ "path": format!("{{ROOT}}/{path}"), "content": content }),
            label: format!("write-{path}"),
        });
        wrote += 1;
    }

    // Reports whether the prelude carried approved working notes. The client's own logs can
    // only say what it meant to send; this is the only place that can say what arrived.
    let notes = if prompt.contains("## Working notes") { "yes" } else { "no" };
    steps.push(Step::Emit(message(
        Some("m2"),
        &format!("wrote {wrote} file(s) [notes: {notes}]"),
    )));
    steps
}

pub fn thought(message_id: Option<&str>, text: &str) -> Value {
    let mut v = json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": text }
    });
    if let Some(m) = message_id {
        v["messageId"] = json!(m);
    }
    v
}

pub fn message(message_id: Option<&str>, text: &str) -> Value {
    let mut v = json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    });
    if let Some(m) = message_id {
        v["messageId"] = json!(m);
    }
    v
}

fn tool_call(id: &str, title: &str, kind: &str, status: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": title,
        "kind": kind,
        "status": status
    })
}

fn tool_done_with_diff(id: &str, path: &str, old: Option<&str>, new: &str) -> Value {
    let mut diff = json!({ "type": "diff", "path": path, "newText": new });
    if let Some(o) = old {
        diff["oldText"] = json!(o);
    }
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "completed",
        "content": [diff],
        "locations": [{ "path": path, "line": 12 }]
    })
}

fn usage(used: u64, size: u64) -> Value {
    json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": size,
        "cost": { "amount": 0.0123, "currency": "USD" }
    })
}

/// The scenario the whole layered-chat design exists for: several thoughts in one turn,
/// separated by tool calls, with the answer last.
fn rich(prompt: &str, turn: u64) -> Vec<Step> {
    vec![
        Step::Emit(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Find the loader", "priority": "high", "status": "in_progress" },
                { "content": "Fix the ordering", "priority": "medium", "status": "pending" }
            ]
        })),
        Step::Emit(thought(Some("m1"), "The prompt mentions ")),
        Step::Emit(thought(Some("m1"), "config loading, so I should start at the loader.")),
        Step::Emit(usage(12_000 + (turn - 1) * 41_000, 200_000)),
        Step::Emit(tool_call("t1", "Read src/config.rs", "read", "in_progress")),
        Step::Emit(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text",
                "text": "pub fn load() -> Config {\n    parse(read_file(PATH))\n}" } }]
        })),
        // The same plan again, in the v2 shape, with the first entry finished and a third added.
        //
        // Two things at once. The nested shape is what v2 sends and what a client reading only the v1
        // position parses as an empty plan — silently, because the discriminator still matches. And
        // sending a *different* set of entries under the same id is what the protocol requires: the
        // client replaces what it had rather than merging, so an entry the agent dropped disappears.
        Step::Emit(json!({
            "sessionUpdate": "plan_update",
            "plan": {
                "type": "items",
                "planId": "main",
                "entries": [
                    { "content": "Find the loader", "priority": "high", "status": "completed" },
                    { "content": "Fix the ordering", "priority": "medium", "status": "in_progress" },
                    { "content": "Run the tests", "priority": "low", "status": "pending" }
                ]
            }
        })),
        // Second thought in the same turn. A naive implementation expands this one *and*
        // the first, which is the bug this whole scenario exists to catch.
        Step::Emit(thought(Some("m2"), "It reads the file twice. I need to see the caller.")),
        Step::Emit(tool_call("t2", "Run cargo test", "execute", "in_progress")),
        Step::Emit(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text",
                "text": "running 3 tests\ntest config::loads ... FAILED\n\nfailures:\n    config::loads" } }]
        })),
        // Third thought, then a permission request before the write.
        Step::Emit(thought(Some("m3"), "Confirmed. I will cache the parse result.")),
        Step::Emit(tool_call("t3", "Edit src/config.rs", "edit", "pending")),
        Step::AskPermission { tool_call_id: "t3".into(), title: "Edit src/config.rs".into() },
        Step::Emit(tool_done_with_diff(
            "t3",
            "/repo/src/config.rs",
            Some("pub fn load() -> Config {\n    parse(read_file(PATH))\n}"),
            "static CACHE: OnceLock<Config> = OnceLock::new();\n\npub fn load() -> &'static Config {\n    CACHE.get_or_init(|| parse(read_file(PATH)))\n}",
        )),
        Step::Emit(usage(53_000 + (turn - 1) * 41_000, 200_000)),
        Step::Emit(message(Some("m4"), "The loader parsed the file on every call. ")),
        Step::Emit(message(Some("m4"), &format!("I cached it behind a OnceLock. (prompt was: {prompt})"))),
    ]
}

/// No messageId anywhere, no usage, no config options. Segmentation has to come entirely
/// from interleaving boundaries.
fn spartan(prompt: &str) -> Vec<Step> {
    vec![
        Step::Emit(thought(None, "no message ids here, ")),
        Step::Emit(thought(None, "so the client has to infer boundaries")),
        Step::Emit(tool_call("s1", "Search for TODO", "search", "in_progress")),
        Step::Emit(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "s1",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "3 matches" } }]
        })),
        Step::Emit(thought(None, "second thought, must not merge with the first")),
        Step::Emit(message(None, &format!("Found 3 TODOs. You asked: {prompt}"))),
    ]
}

fn resume() -> Vec<Step> {
    vec![
        Step::Emit(thought(Some("m1"), "beginning a thought, ")),
        Step::Emit(tool_call("r1", "Read file", "read", "in_progress")),
        Step::Emit(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "r1", "status": "completed"
        })),
        // Same messageId again: this is one thought interrupted, not two thoughts.
        Step::Emit(thought(Some("m1"), "and now finishing the same thought.")),
        Step::Emit(message(Some("m2"), "done")),
    ]
}

fn stall() -> Vec<Step> {
    vec![
        Step::Emit(thought(Some("m1"), "about to start something long")),
        Step::Emit(tool_call("long1", "Run the full suite", "execute", "in_progress")),
        Step::Sleep(600),
        Step::Emit(message(Some("m2"), "never reached under cancellation")),
    ]
}

/// Things the schema does not describe. The client must record them and carry on.
fn alien() -> Vec<Step> {
    vec![
        Step::Emit(thought(Some("m1"), "ordinary thought first")),
        Step::Emit(json!({
            "sessionUpdate": "quantum_entanglement_update",
            "spookiness": 11,
            "note": "this discriminant does not exist in any ACP version"
        })),
        Step::Emit(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "some_future_content_kind", "payload": { "a": 1 } },
            "messageId": "m2"
        })),
        Step::Emit(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "a1",
            "title": "A tool of unknown kind",
            "kind": "telepathy",
            "status": "in_progress"
        })),
        Step::Emit(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "a1",
            "status": "supernova"
        })),
        Step::Emit(message(Some("m3"), "survived the alien updates")),
    ]
}

fn flood() -> Vec<Step> {
    let mut steps = vec![Step::Emit(thought(Some("f0"), "flooding: "))];
    for i in 0..400 {
        steps.push(Step::Emit(thought(Some("f0"), &format!("{i} "))));
    }
    for i in 0..200 {
        steps.push(Step::Emit(message(Some("f1"), &format!("token{i} "))));
    }
    steps
}
