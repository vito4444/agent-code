//! Remembered permission decisions, bound to what was actually approved.
//!
//! # What binding to a name costs
//!
//! CVE-2025-54136 ("MCPoison") bound an approval to the *name* of an MCP server entry.
//! Approve `build-tool` once, and the attacker rewrites what `build-tool` runs; every
//! later launch of the project executes the new command with no prompt, because the name
//! still matches. The fix that shipped in Cursor 1.3 was to hash the whole entry, so that
//! adding a single space forces a fresh approval, and to show the user the old and new
//! versions side by side.
//!
//! So the key here is `(tool_kind, content_hash)` and the hash covers everything that
//! determines what the operation will do. A remembered approval that survives a change to
//! the operation is not a memory of a decision, it is a blank cheque.
//!
//! # Why the classification is ours and not the protocol's
//!
//! ACP does not connect a permission prompt to a category of operation. `options` is a
//! list the agent constructs from scratch on every request, with agent-chosen ids and
//! agent-chosen labels, and nothing in the schema ties any of it to the `ToolKind` of the
//! tool call. There is therefore no protocol-level notion of "the same kind of thing I
//! approved last time" to reuse — the client has to impose one, which is what
//! [`PermissionKey::tool_kind`] is. Treating the agent's `optionId` as an identity would
//! let the agent choose which approvals it inherits.
//!
//! # Evaluation order
//!
//! Deny, then ask, then allow; the first match wins. This is the order Claude Code uses,
//! and the important half is that a deny cannot be undone by an allow in a narrower
//! scope — otherwise "never let anything run `rm -rf`" is one careless session-scoped
//! click away from being untrue.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// What the user decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// How widely a remembered decision applies.
///
/// The variants carry their parents so that a lookup can evaluate the whole chain without
/// a separate registry mapping sessions to projects — which would be one more thing that
/// could disagree with reality at exactly the wrong moment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// Forgotten when the session ends.
    Session { project: String, session: String },
    /// Applies to every session in one project.
    Project { project: String },
    /// Applies everywhere. Reserve for decisions that are genuinely about the tool rather
    /// than about the code being worked on.
    Global,
}

impl Scope {
    pub fn session(project: impl Into<String>, session: impl Into<String>) -> Scope {
        Scope::Session {
            project: project.into(),
            session: session.into(),
        }
    }

    pub fn project(project: impl Into<String>) -> Scope {
        Scope::Project {
            project: project.into(),
        }
    }

    /// The next wider scope, or `None` at [`Scope::Global`].
    pub fn parent(&self) -> Option<Scope> {
        match self {
            Scope::Session { project, .. } => Some(Scope::Project {
                project: project.clone(),
            }),
            Scope::Project { .. } => Some(Scope::Global),
            Scope::Global => None,
        }
    }

    /// This scope and every scope containing it, narrowest first.
    pub fn chain(&self) -> Vec<Scope> {
        let mut chain = vec![self.clone()];
        let mut current = self.clone();
        while let Some(parent) = current.parent() {
            chain.push(parent.clone());
            current = parent;
        }
        chain
    }

    /// Smaller is narrower. Used only for reporting which scope decided.
    pub fn breadth(&self) -> u8 {
        match self {
            Scope::Session { .. } => 0,
            Scope::Project { .. } => 1,
            Scope::Global => 2,
        }
    }
}

/// Identifies an approval.
///
/// `tool_kind` is the client's own classification (`fs_write`, `execute`, `mcp_call`, …);
/// `content_hash` is [`hash_decision_content`] over everything that decides what the
/// operation does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionKey {
    pub tool_kind: String,
    pub content_hash: String,
}

impl PermissionKey {
    pub fn new(tool_kind: impl Into<String>, content_hash: impl Into<String>) -> Self {
        PermissionKey {
            tool_kind: tool_kind.into(),
            content_hash: content_hash.into(),
        }
    }

    /// Convenience for the common case of hashing the content at the same moment.
    pub fn for_content(tool_kind: impl Into<String>, parts: &[&[u8]]) -> Self {
        PermissionKey {
            tool_kind: tool_kind.into(),
            content_hash: hash_decision_content(parts),
        }
    }
}

/// SHA-256 over the parts that determine what an operation will do.
///
/// Each part is length-prefixed and the whole thing is domain-separated. Concatenating
/// the parts instead would make `["rm -rf", " /tmp"]` and `["rm -rf ", "/tmp"]` hash the
/// same, which turns "approve this exact command" into "approve any command whose fields
/// happen to concatenate the same way" — the field-boundary version of the same bug this
/// module exists to prevent.
///
/// The caller normalises: the bytes passed in are what the user is shown, so any
/// normalisation applied here and not there would reintroduce the gap between the two.
pub fn hash_decision_content(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"wkbd-sec/permission/v1\0");
    hasher.update((parts.len() as u64).to_le_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

/// One remembered decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecord {
    pub scope: Scope,
    pub key: PermissionKey,
    pub decision: Decision,
    pub recorded_at_unix_ms: u64,
}

/// What [`PermissionStore::remember`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberOutcome {
    Stored,
    /// Replaced an earlier decision for the same scope and key.
    Replaced,
    /// **Not** stored: a deny in this scope or a wider one already covers this key.
    ///
    /// Reported rather than silently accepted so the UI can say why the click did not
    /// take effect. Revoking a deny is a separate, explicit act ([`PermissionStore::forget`]),
    /// because "I approve this once" must never be a way to erase "never allow this".
    ShadowedByDeny,
}

/// Which scope produced the answer, for the audit log and for "why was I not asked?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub decision: Decision,
    pub scope: Scope,
}

/// In-memory store of remembered decisions, serialisable as it stands.
///
/// A flat record list rather than nested maps: the whole point is that lookups consult
/// several scopes and that deny wins across all of them, and a shape that makes the
/// "just check this one bucket" mistake easy to write is the wrong shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStore {
    records: Vec<PermissionRecord>,
}

impl PermissionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a decision for exactly this scope.
    pub fn remember(
        &mut self,
        scope: Scope,
        key: PermissionKey,
        decision: Decision,
    ) -> RememberOutcome {
        // Checked before anything is written: overwriting the deny record and *then*
        // looking for it would find nothing, which is how a same-scope allow would
        // quietly erase a deny.
        if decision == Decision::Allow && self.deny_in_chain(&scope, &key).is_some() {
            return RememberOutcome::ShadowedByDeny;
        }

        let existing = self
            .records
            .iter()
            .position(|r| r.scope == scope && r.key == key);

        let record = PermissionRecord {
            scope: scope.clone(),
            key: key.clone(),
            decision,
            recorded_at_unix_ms: now_ms(),
        };

        match existing {
            Some(index) => {
                self.records[index] = record;
                RememberOutcome::Replaced
            }
            None => {
                self.records.push(record);
                RememberOutcome::Stored
            }
        }
    }

    /// The effective decision for a request made in `scope`, or `None` when the user has
    /// to be asked.
    pub fn lookup(&self, scope: &Scope, key: &PermissionKey) -> Option<Decision> {
        self.resolve(scope, key).map(|r| r.decision)
    }

    /// As [`Self::lookup`], but also says which scope decided.
    pub fn resolve(&self, scope: &Scope, key: &PermissionKey) -> Option<Resolution> {
        // Deny first, across every applicable scope. Evaluating narrowest-first and
        // returning the first hit of any kind would let a session-scoped allow override a
        // global deny.
        if let Some(scope) = self.deny_in_chain(scope, key) {
            return Some(Resolution {
                decision: Decision::Deny,
                scope,
            });
        }
        for candidate in scope.chain() {
            if let Some(record) = self.find(&candidate, key) {
                if record.decision == Decision::Allow {
                    return Some(Resolution {
                        decision: Decision::Allow,
                        scope: candidate,
                    });
                }
            }
        }
        None
    }

    /// Drops one exact record. Widening or narrowing is the caller's business.
    pub fn forget(&mut self, scope: &Scope, key: &PermissionKey) -> bool {
        let before = self.records.len();
        self.records
            .retain(|r| !(r.scope == *scope && r.key == *key));
        self.records.len() != before
    }

    /// Drops every record for a session. Session-scoped decisions must not outlive the
    /// session that made them.
    pub fn forget_session(&mut self, project: &str, session: &str) -> usize {
        let before = self.records.len();
        self.records.retain(|r| {
            !matches!(&r.scope, Scope::Session { project: p, session: s } if p == project && s == session)
        });
        before - self.records.len()
    }

    pub fn records(&self) -> &[PermissionRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn find(&self, scope: &Scope, key: &PermissionKey) -> Option<&PermissionRecord> {
        self.records
            .iter()
            .find(|r| r.scope == *scope && r.key == *key)
    }

    fn deny_in_chain(&self, scope: &Scope, key: &PermissionKey) -> Option<Scope> {
        scope.chain().into_iter().find(|candidate| {
            self.find(candidate, key)
                .is_some_and(|r| r.decision == Decision::Deny)
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp_entry(command: &str) -> PermissionKey {
        // Everything that decides what the entry does: the server name alone is what
        // MCPoison proved insufficient.
        PermissionKey::for_content("mcp_launch", &[b"build-tool", command.as_bytes()])
    }

    #[test]
    fn a_one_byte_change_invalidates_a_remembered_approval() {
        // The MCPoison regression test. The approval is for `npm run build`; the attacker
        // rewrites the entry afterwards.
        let mut store = PermissionStore::new();
        let scope = Scope::project("/srv/project");
        let approved = mcp_entry("npm run build");
        store.remember(scope.clone(), approved.clone(), Decision::Allow);

        assert_eq!(store.lookup(&scope, &approved), Some(Decision::Allow));

        for tampered in [
            "npm run build ",           // one trailing space, the Cursor 1.3 example
            "npm  run build",           // one extra space in the middle
            "npm run build; curl evil", // the actual attack
            "npm run buila",
        ] {
            assert_eq!(
                store.lookup(&scope, &mcp_entry(tampered)),
                None,
                "approval survived a change to {tampered:?}"
            );
        }
    }

    #[test]
    fn the_same_content_under_a_different_tool_kind_is_a_different_decision() {
        let mut store = PermissionStore::new();
        let scope = Scope::project("/srv/project");
        let content: &[&[u8]] = &[b"/srv/project/deploy.sh"];

        let read = PermissionKey::for_content("fs_read", content);
        let execute = PermissionKey::for_content("execute", content);
        assert_eq!(read.content_hash, execute.content_hash);

        store.remember(scope.clone(), read.clone(), Decision::Allow);
        assert_eq!(store.lookup(&scope, &read), Some(Decision::Allow));
        assert_eq!(
            store.lookup(&scope, &execute),
            None,
            "approval to read a file must not authorise executing it"
        );
    }

    #[test]
    fn deny_cannot_be_overridden_by_an_allow_in_any_scope() {
        let key = mcp_entry("rm -rf /");
        let session = Scope::session("/srv/project", "session-1");
        let project = Scope::project("/srv/project");

        // Wider deny, narrower allow.
        let mut store = PermissionStore::new();
        store.remember(Scope::Global, key.clone(), Decision::Deny);
        let outcome = store.remember(session.clone(), key.clone(), Decision::Allow);
        assert_eq!(outcome, RememberOutcome::ShadowedByDeny);
        assert_eq!(store.lookup(&session, &key), Some(Decision::Deny));
        assert_eq!(store.lookup(&project, &key), Some(Decision::Deny));
        assert_eq!(store.lookup(&Scope::Global, &key), Some(Decision::Deny));

        // Same scope, allow written after deny.
        let mut store = PermissionStore::new();
        store.remember(project.clone(), key.clone(), Decision::Deny);
        assert_eq!(
            store.remember(project.clone(), key.clone(), Decision::Allow),
            RememberOutcome::ShadowedByDeny
        );
        assert_eq!(store.lookup(&project, &key), Some(Decision::Deny));
        // The shadowed allow was not written at all, so there is no contradictory pair on
        // disk waiting for a future reader to resolve differently.
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.records()[0].decision, Decision::Deny);

        // Revoking is explicit, and only then can the allow be recorded.
        assert!(store.forget(&project, &key));
        assert_eq!(
            store.remember(project.clone(), key.clone(), Decision::Allow),
            RememberOutcome::Stored
        );
        assert_eq!(store.lookup(&project, &key), Some(Decision::Allow));

        // Narrower deny does not leak outwards: another session is unaffected.
        let mut store = PermissionStore::new();
        store.remember(Scope::Global, key.clone(), Decision::Allow);
        store.remember(session.clone(), key.clone(), Decision::Deny);
        assert_eq!(store.lookup(&session, &key), Some(Decision::Deny));
        assert_eq!(
            store.lookup(&Scope::session("/srv/project", "session-2"), &key),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn deny_can_be_recorded_at_every_scope() {
        let key = mcp_entry("curl | sh");
        for scope in [
            Scope::session("/p", "s"),
            Scope::project("/p"),
            Scope::Global,
        ] {
            let mut store = PermissionStore::new();
            store.remember(scope.clone(), key.clone(), Decision::Deny);
            assert_eq!(store.lookup(&scope, &key), Some(Decision::Deny));
        }
    }

    #[test]
    fn scope_precedence_runs_narrow_to_wide() {
        let key = mcp_entry("npm test");
        let mut store = PermissionStore::new();

        let s1 = Scope::session("/p1", "s1");
        let s2 = Scope::session("/p1", "s2");
        let other_project = Scope::session("/p2", "s3");

        assert_eq!(store.lookup(&s1, &key), None);

        store.remember(Scope::project("/p1"), key.clone(), Decision::Allow);
        assert_eq!(store.lookup(&s1, &key), Some(Decision::Allow));
        assert_eq!(store.lookup(&s2, &key), Some(Decision::Allow));
        assert_eq!(
            store.lookup(&other_project, &key),
            None,
            "a project-scoped approval must not reach another project"
        );

        // The resolution names the scope that decided, which is what the UI shows when
        // the user asks why they were not prompted.
        let resolved = store.resolve(&s1, &key).unwrap();
        assert_eq!(resolved.scope, Scope::project("/p1"));

        store.remember(Scope::Global, key.clone(), Decision::Allow);
        assert_eq!(
            store.lookup(&other_project, &key),
            Some(Decision::Allow),
            "a global approval reaches every project"
        );
    }

    #[test]
    fn session_decisions_can_be_dropped_when_the_session_ends() {
        let key = mcp_entry("npm test");
        let mut store = PermissionStore::new();
        let s1 = Scope::session("/p1", "s1");
        store.remember(s1.clone(), key.clone(), Decision::Allow);
        store.remember(Scope::project("/p1"), mcp_entry("other"), Decision::Allow);

        assert_eq!(store.forget_session("/p1", "s1"), 1);
        assert_eq!(store.lookup(&s1, &key), None);
        assert_eq!(store.records().len(), 1);
    }

    #[test]
    fn hashing_is_domain_separated_and_field_boundaries_are_preserved() {
        let split_one = hash_decision_content(&[b"rm -rf", b" /tmp"]);
        let split_two = hash_decision_content(&[b"rm -rf ", b"/tmp"]);
        assert_ne!(
            split_one, split_two,
            "field boundaries must change the hash, or fields can be shuffled between \
             each other without invalidating an approval"
        );

        assert_ne!(
            hash_decision_content(&[b"a"]),
            hash_decision_content(&[b"a", b""]),
            "an added empty field must change the hash"
        );
        assert_eq!(
            hash_decision_content(&[b"a", b"b"]),
            hash_decision_content(&[b"a", b"b"])
        );
        // Hex-encoded SHA-256.
        assert_eq!(hash_decision_content(&[]).len(), 64);
        assert!(hash_decision_content(&[b"x"])
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_store_round_trips_through_json() {
        let mut store = PermissionStore::new();
        let key = mcp_entry("npm test");
        store.remember(Scope::session("/p", "s"), key.clone(), Decision::Allow);
        store.remember(Scope::Global, mcp_entry("rm -rf /"), Decision::Deny);

        let json = serde_json::to_string(&store).unwrap();
        let back: PermissionStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, store);
        assert_eq!(
            back.lookup(&Scope::session("/p", "s"), &key),
            Some(Decision::Allow)
        );
        // Deny precedence is a property of evaluation, so it survives a reload even if a
        // contradictory record is written into the file by hand.
        assert_eq!(
            back.lookup(&Scope::session("/p", "s"), &mcp_entry("rm -rf /")),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn a_hand_edited_store_still_denies() {
        // Defence in depth for the case the outcome reporting cannot cover: a file where
        // an allow and a deny coexist, however it got that way.
        let key = mcp_entry("rm -rf /");
        let json = serde_json::json!({
            "records": [
                {
                    "scope": {"kind": "session", "project": "/p", "session": "s"},
                    "key": key,
                    "decision": "allow",
                    "recorded_at_unix_ms": 1
                },
                {
                    "scope": {"kind": "global"},
                    "key": key,
                    "decision": "deny",
                    "recorded_at_unix_ms": 2
                }
            ]
        });
        let store: PermissionStore = serde_json::from_value(json).unwrap();
        assert_eq!(
            store.lookup(&Scope::session("/p", "s"), &key),
            Some(Decision::Deny)
        );
    }
}
