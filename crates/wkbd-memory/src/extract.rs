//! Extracting facts from a finished run.
//!
//! Nobody fills in a form. Facts come from the event log of a run that has already happened,
//! which means the raw material is what the agent did and what happened as a result, not what
//! anyone said they intended.
//!
//! The type in this module is the first of the three layers that keep user rules separate
//! from inferred memory: [`ExtractedKind`] has no variant for a user rule, so the extractor
//! cannot produce one. Not "does not", cannot — there is no name for it here.

use serde::{Deserialize, Serialize};
use wkbd_proto::{Event, EventPayload, ToolContent, ToolKind};

use crate::facts::{NewFact, SourceTrust};

/// The only kinds of thing extraction can produce.
///
/// Deliberately does not include a user rule. Adding one here would be the change that breaks
/// the separation, which is why the type is small and this comment is attached to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedKind {
    /// A command that was run and what it did.
    CommandOutcome,
    /// A file the run modified.
    FileTouched,
    /// A tool that failed.
    Failure,
    /// A permission decision the user made, recorded as evidence of preference rather than as
    /// a rule. Even an explicit "always allow" is evidence about one operation, not a standing
    /// instruction, and promoting it to a rule would put words in the user's mouth.
    PermissionPreference,
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub kind: ExtractedKind,
    pub subject: String,
    pub predicate: String,
    pub body: String,
    pub confidence: f64,
}

/// Reads a run's events and proposes facts.
///
/// Entirely deterministic: no model is involved. That is a deliberate limit rather than a
/// stopgap. The strong claim for this kind of learning rests on having a verifiable signal —
/// a test passed, a command exited non-zero — and where there is no such signal the reported
/// benefit collapses to roughly nothing. So this extracts only what the log directly witnessed
/// and leaves interpretation to a later, explicitly-gated step.
pub fn from_events(events: &[Event], project_root: Option<&str>) -> Vec<NewFact> {
    let mut out = Vec::new();
    let run = events.first().map(|e| e.session_id.clone());

    let mut open_tools: std::collections::HashMap<String, (String, ToolKind)> =
        std::collections::HashMap::new();

    for event in events {
        match &event.payload {
            EventPayload::ToolCallStarted { tool_call_id, title, kind, .. } => {
                open_tools.insert(tool_call_id.clone(), (title.clone(), kind.clone()));
            }
            EventPayload::ToolCallUpdated { tool_call_id, status, content, .. } => {
                let Some((title, kind)) = open_tools.get(tool_call_id) else { continue };

                if matches!(status, Some(wkbd_proto::ToolStatus::Failed)) {
                    out.push(NewFact {
                        project_root: project_root.map(str::to_string),
                        subject: format!("tool:{}", tool_kind_str(kind)),
                        predicate: "fails_for".into(),
                        body: format!("{title} failed during this run"),
                        // Low confidence on purpose: one failure is an observation, not a
                        // pattern, and the counters in the playbook are where repetition
                        // turns into a belief.
                        confidence: 0.3,
                        valid_at: Some(event.at_ms),
                        source_run: run.clone(),
                        source_trust: SourceTrust::Internal,
                    });
                }

                for c in content {
                    if let ToolContent::Diff { path, .. } = c {
                        out.push(NewFact {
                            project_root: project_root.map(str::to_string),
                            subject: format!("file:{path}"),
                            predicate: "was_modified_by".into(),
                            body: format!("{path} was modified while: {title}"),
                            confidence: 0.6,
                            valid_at: Some(event.at_ms),
                            source_run: run.clone(),
                            source_trust: SourceTrust::Internal,
                        });
                    }
                }
            }
            // A write we performed on the agent's behalf. Stronger evidence than a diff reported
            // inside a tool call: we did this one ourselves, through the path guard, so the record is
            // of an action rather than of a claim about an action. The extractor predates this event
            // and did not look at it, which meant that for an agent using the protocol's file
            // methods — the case the daemon now encourages — nothing was learned at all.
            EventPayload::FileAccess { op, requested, resolved, allowed, .. } => {
                if !*allowed || *op != wkbd_proto::FileOp::Write {
                    continue;
                }
                let path = resolved.clone().unwrap_or_else(|| requested.clone());
                out.push(NewFact {
                    project_root: project_root.map(str::to_string),
                    subject: format!("file:{path}"),
                    predicate: "was_modified_by".into(),
                    body: format!("{path} was written during this run"),
                    // Higher than the diff case: that one is the agent describing a change, this one
                    // is us having made it.
                    confidence: 0.7,
                    valid_at: Some(event.at_ms),
                    source_run: run.clone(),
                    source_trust: SourceTrust::Internal,
                });
            }
            EventPayload::PermissionResolved { option_id, auto, .. } => {
                if *auto {
                    continue;
                }
                if let Some(option_id) = option_id {
                    out.push(NewFact {
                        project_root: project_root.map(str::to_string),
                        subject: "user".into(),
                        predicate: "answered_permission".into(),
                        body: format!("chose {option_id} for a permission request"),
                        confidence: 0.5,
                        valid_at: Some(event.at_ms),
                        source_run: run.clone(),
                        source_trust: SourceTrust::Internal,
                    });
                }
            }
            _ => {}
        }
    }

    out
}

fn tool_kind_str(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Unknown(s) => s.clone(),
        other => format!("{other:?}").to_lowercase(),
    }
}

#[cfg(test)]
mod file_access_tests {
    use super::*;
    use wkbd_proto::{FileOp, PendingEvent};

    fn event(payload: EventPayload) -> Event {
        let pending = PendingEvent::new("run:abc".to_string(), payload);
        Event { seq: 1, session_id: pending.session_id, at_ms: 1_000, payload: pending.payload }
    }

    fn write(requested: &str, allowed: bool) -> Event {
        event(EventPayload::FileAccess {
            op: FileOp::Write,
            requested: requested.to_string(),
            resolved: Some(requested.to_string()),
            allowed,
            refusal: (!allowed).then(|| "outside-root".to_string()),
            bytes: Some(10),
        })
    }

    /// The seam this closes. The extractor was written before the daemon performed file I/O on an
    /// agent's behalf, so for an agent using the protocol's file methods — the case the daemon now
    /// encourages, because it is the only one where the access is bounded and logged — nothing was
    /// learned from a run at all.
    #[test]
    fn a_write_we_performed_is_a_fact() {
        let facts = from_events(&[write("/p/src/a.rs", true)], Some("/p"));
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert_eq!(facts[0].subject, "file:/p/src/a.rs");
        assert_eq!(facts[0].predicate, "was_modified_by");
        assert_eq!(facts[0].source_trust, SourceTrust::Internal);
    }

    /// A refusal is not a modification. Learning "this file was modified" from an attempt that was
    /// blocked would record the opposite of what happened.
    #[test]
    fn a_refused_write_teaches_nothing() {
        assert!(from_events(&[write("/etc/passwd", false)], Some("/p")).is_empty());
    }

    #[test]
    fn a_read_is_not_a_modification() {
        let e = event(EventPayload::FileAccess {
            op: FileOp::Read,
            requested: "/p/src/a.rs".into(),
            resolved: Some("/p/src/a.rs".into()),
            allowed: true,
            refusal: None,
            bytes: Some(10),
        });
        assert!(from_events(&[e], Some("/p")).is_empty());
    }

    /// Nothing the extractor produces may claim the user said it. There is no name for that here —
    /// `ExtractedKind` has no variant for a rule — and this asserts the property at the output as
    /// well, because the vocabulary argument only holds while nobody adds a field by hand.
    #[test]
    fn nothing_extracted_is_attributed_to_the_user() {
        let facts = from_events(&[write("/p/a", true), write("/p/b", true)], Some("/p"));
        assert!(!facts.is_empty());
        for f in &facts {
            assert_ne!(f.source_trust, SourceTrust::User, "{f:?}");
        }
    }
}
