//! Performing file I/O on an agent's behalf.
//!
//! The protocol has the *client* do this: the agent sends an absolute path and we open it. It
//! defines no boundary — no working-directory constraint, no sandbox semantics — and it requires
//! us to create a file that does not exist. The same class of defect has shipped in four separate
//! products, and every time the shape was "resolution failed, so fall back to comparing strings".
//!
//! ## Why offer it at all
//!
//! Not offering it is not the safe option, which is worth being explicit about because it looks
//! like it should be. An agent that cannot ask us to read a file reads it itself, in its own
//! process, with whatever privileges that process has — and we see nothing, log nothing and
//! enforce nothing. Routing the request through here puts every access behind a boundary decided
//! in the kernel and into an append-only log with a stable refusal reason.
//!
//! So this is offered by default, and `--no-client-fs` exists for anyone who would rather the
//! agent do its own I/O. The trade is observability and enforcement against a smaller surface.
//!
//! ## What makes it safe
//!
//! `wkbd-sec::path_guard`, which resolves with `openat2` under
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` so the decision happens in the
//! kernel with no window between checking and using, and refuses rather than falling back when
//! resolution fails. Nothing here re-implements any part of that decision; it converts a protocol
//! request into a guard call and a protocol response into an audit record.

use serde_json::{json, Value};
use std::io::{Read, Seek, Write};
use std::path::Path;
use wkbd_proto::{EventPayload, FileOp};
use wkbd_sec::path_guard::{GuardError, PathGuard};

/// The outcome of one request: what to send back, and what to record.
pub struct Outcome {
    pub reply: Result<Value, (i64, String)>,
    pub event: EventPayload,
}

/// Largest file we will hand to an agent in one response.
///
/// A bound rather than a guess at what is reasonable: without one, an agent asking for a
/// multi-gigabyte file allocates that much in the daemon and takes the whole workbench with it.
/// The protocol has no paging for this, so the honest answer past the limit is an error naming the
/// size rather than a silent truncation that looks like a short file.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

pub fn read_text_file(guard: &PathGuard, params: &Value) -> Outcome {
    let Some(requested) = params.get("path").and_then(|p| p.as_str()) else {
        return Outcome {
            reply: Err((-32602, "fs/read_text_file requires a path".into())),
            event: refusal_event(FileOp::Read, "", "missing-path"),
        };
    };

    // Both optional in the protocol, and both are a window into the file rather than a
    // constraint we need to check: the boundary is decided by the path alone.
    let line = params.get("line").and_then(|v| v.as_u64());
    let limit = params.get("limit").and_then(|v| v.as_u64());

    let path = Path::new(requested);
    let mut file = match guard.open_read(path) {
        Ok(f) => f,
        Err(e) => return Outcome::from_guard_error(FileOp::Read, requested, e),
    };

    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size > MAX_READ_BYTES && limit.is_none() {
        return Outcome {
            reply: Err((
                -32000,
                format!(
                    "{requested} is {size} bytes, over the {MAX_READ_BYTES} byte limit for one \
                     response; ask for a line range"
                ),
            )),
            event: refusal_event(FileOp::Read, requested, "too-large"),
        };
    }

    let mut content = String::new();
    if let Err(e) = file.read_to_string(&mut content) {
        return Outcome {
            reply: Err((-32000, format!("reading {requested}: {e}"))),
            event: refusal_event(FileOp::Read, requested, "io-error"),
        };
    }

    let sliced = match (line, limit) {
        (None, None) => content,
        _ => {
            // `line` is 1-based in the protocol.
            let start = line.unwrap_or(1).saturating_sub(1) as usize;
            let take = limit.unwrap_or(u64::MAX) as usize;
            content
                .lines()
                .skip(start)
                .take(take)
                .collect::<Vec<_>>()
                .join("\n")
        }
    };

    let resolved = guard.resolve_for_display(path).ok().map(|p| p.display().to_string());
    let bytes = sliced.len() as u64;

    Outcome {
        reply: Ok(json!({ "content": sliced })),
        event: EventPayload::FileAccess {
            op: FileOp::Read,
            requested: requested.to_string(),
            resolved,
            allowed: true,
            refusal: None,
            bytes: Some(bytes),
        },
    }
}

pub fn write_text_file(guard: &PathGuard, params: &Value) -> Outcome {
    let Some(requested) = params.get("path").and_then(|p| p.as_str()) else {
        return Outcome {
            reply: Err((-32602, "fs/write_text_file requires a path".into())),
            event: refusal_event(FileOp::Write, "", "missing-path"),
        };
    };
    let Some(content) = params.get("content").and_then(|c| c.as_str()) else {
        return Outcome {
            reply: Err((-32602, "fs/write_text_file requires content".into())),
            event: refusal_event(FileOp::Write, requested, "missing-content"),
        };
    };

    let path = Path::new(requested);
    // The protocol requires the client to create the file when it does not exist, which means
    // "the target is missing" is the normal path here rather than an error — and it is exactly
    // the condition under which the published defects in this area triggered.
    let mut file = match guard.open_write_create(path) {
        Ok(f) => f,
        Err(e) => return Outcome::from_guard_error(FileOp::Write, requested, e),
    };

    if let Err(e) = file
        .write_all(content.as_bytes())
        .and_then(|_| file.flush())
        .and_then(|_| file.stream_position())
    {
        return Outcome {
            reply: Err((-32000, format!("writing {requested}: {e}"))),
            event: refusal_event(FileOp::Write, requested, "io-error"),
        };
    }

    let resolved = guard.resolve_for_display(path).ok().map(|p| p.display().to_string());

    Outcome {
        reply: Ok(json!({})),
        event: EventPayload::FileAccess {
            op: FileOp::Write,
            requested: requested.to_string(),
            resolved,
            allowed: true,
            refusal: None,
            bytes: Some(content.len() as u64),
        },
    }
}

impl Outcome {
    fn from_guard_error(op: FileOp, requested: &str, error: GuardError) -> Outcome {
        let kind = error.audit_kind();
        // A refusal for being outside the boundary and a refusal for the file not existing are
        // different answers to the agent: the first is a policy decision it should not retry, the
        // second is ordinary. Collapsing them would have an agent retrying a path it will never
        // be allowed to reach.
        let code = if error.is_not_found() { -32001 } else { -32003 };
        Outcome {
            reply: Err((code, format!("{requested}: {error}"))),
            event: EventPayload::FileAccess {
                op,
                requested: requested.to_string(),
                resolved: None,
                allowed: false,
                refusal: Some(kind.to_string()),
                bytes: None,
            },
        }
    }
}

fn refusal_event(op: FileOp, requested: &str, kind: &str) -> EventPayload {
    EventPayload::FileAccess {
        op,
        requested: requested.to_string(),
        resolved: None,
        allowed: false,
        refusal: Some(kind.to_string()),
        bytes: None,
    }
}

/// The capabilities to declare at `initialize`.
///
/// Declaring nothing means the agent will not ask, which is not the same as it not doing the I/O.
pub fn client_capabilities(offer_fs: bool) -> Value {
    if offer_fs {
        json!({ "fs": { "readTextFile": true, "writeTextFile": true } })
    } else {
        json!({ "fs": { "readTextFile": false, "writeTextFile": false } })
    }
}
