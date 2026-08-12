//! User rules.
//!
//! A rule is something the user typed and expects to be obeyed. A memory is something the
//! system inferred and may have got wrong. They are stored in the same table but they are not
//! the same kind of record, and the separation is enforced in three independent places:
//!
//! 1. **Vocabulary.** The extractor's output type ([`crate::extract::ExtractedKind`]) has no
//!    variant for a user rule. It cannot name one, so it cannot emit one — not as a bug, but
//!    as a thing that does not typecheck.
//! 2. **Storage.** `facts.kind` has a CHECK constraint, and the only code path that writes
//!    `'user_rule'` is in this module, which the extractor does not call.
//! 3. **Queries.** Every consolidation, supersession and decay query filters
//!    `kind = 'inferred'`. A rule is not a candidate for any of them.
//!
//! One layer would be enough if nobody ever edited this code again. Three layers is the
//! answer to "what happens when someone adds a fourth writer in a year". The consequence of
//! getting it wrong is specific and bad: a preference that a model can rewrite, or that
//! confidence decay can quietly retire, is a setting the program has stopped honouring while
//! the settings screen still shows it as enabled. That is strictly worse than not having the
//! feature at all.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wkbd_store::Store;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleScope {
    /// Applies to every project on this machine.
    Global,
    /// Applies to one project only.
    Project(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRule {
    pub scope: RuleScope,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    pub scope: String,
    pub project_root: Option<String>,
    pub body: String,
    pub enabled: bool,
    /// Session entry points observed applying this rule.
    ///
    /// Recorded rather than asserted. Rules silently not applying in one situation — a
    /// resumed session, a worker the orchestrator started, a session restarted to change a
    /// model — is the classic failure, so the interface shows which entry points have
    /// actually been seen using it instead of claiming they all do.
    pub applied_at: Vec<String>,
}

/// The literal string stored in `facts.kind` for a rule. Deliberately a constant in this
/// module: the extractor does not import it and has no reason to.
const KIND_USER_RULE: &str = "user_rule";

pub async fn create(store: &Store, rule: NewRule) -> Result<Rule> {
    let id = uuid::Uuid::new_v4().to_string();
    let (scope, project_root) = match &rule.scope {
        RuleScope::Global => ("global".to_string(), None),
        RuleScope::Project(root) => ("project".to_string(), Some(root.clone())),
    };

    let body = rule.body.trim().to_string();
    if body.is_empty() {
        anyhow::bail!("a rule cannot be empty");
    }

    let inserted = Rule {
        id: id.clone(),
        scope: scope.clone(),
        project_root: project_root.clone(),
        body: body.clone(),
        enabled: true,
        applied_at: Vec::new(),
    };

    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO facts
                 (id, kind, scope, project_root, subject, predicate, body,
                  confidence, created_ms, source_trust, enabled)
                 VALUES (?1, ?2, ?3, ?4, 'user', 'requires', ?5, NULL, ?6, 'user', 1)",
                rusqlite::params![
                    id,
                    KIND_USER_RULE,
                    scope,
                    project_root,
                    body,
                    wkbd_store::now_ms()
                ],
            )?;
            Ok(())
        })
        .await?;

    Ok(inserted)
}

pub async fn list(store: &Store, project_root: Option<&str>) -> Result<Vec<Rule>> {
    let project_root = project_root.map(str::to_string);
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, project_root, body, enabled FROM facts
                 WHERE kind = ?1
                   AND expired_ms IS NULL
                   AND (scope = 'global' OR project_root IS ?2)
                 ORDER BY scope DESC, created_ms",
            )?;
            let rows = stmt.query_map(rusqlite::params![KIND_USER_RULE, project_root], |row| {
                Ok(Rule {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    project_root: row.get(2)?,
                    body: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    applied_at: Vec::new(),
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Soft-deletes a rule.
///
/// The row is expired rather than removed for the same reason inferred facts are: deletion
/// makes "we never had this rule" and "we had it and it was removed" indistinguishable, and
/// the second is the one someone will ask about.
pub async fn delete(store: &Store, id: &str) -> Result<()> {
    let id = id.to_string();
    store
        .write(move |tx| {
            let changed = tx.execute(
                "UPDATE facts SET expired_ms = ?2, enabled = 0
                 WHERE id = ?1 AND kind = ?3",
                rusqlite::params![id, wkbd_store::now_ms(), KIND_USER_RULE],
            )?;
            if changed == 0 {
                anyhow::bail!("no such rule");
            }
            Ok(())
        })
        .await
}

pub async fn set_enabled(store: &Store, id: &str, enabled: bool) -> Result<()> {
    let id = id.to_string();
    store
        .write(move |tx| {
            tx.execute(
                "UPDATE facts SET enabled = ?2 WHERE id = ?1 AND kind = ?3",
                rusqlite::params![id, i64::from(enabled), KIND_USER_RULE],
            )?;
            Ok(())
        })
        .await
}

/// The rules that apply to a project, in injection order.
///
/// Global first, then project, because the more specific rule should be read last where it
/// carries more weight in the model's attention.
pub async fn applicable(store: &Store, project_root: &str) -> Result<Vec<String>> {
    let project_root = project_root.to_string();
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT body FROM facts
                 WHERE kind = ?1 AND enabled = 1 AND expired_ms IS NULL
                   AND (scope = 'global' OR project_root = ?2)
                 ORDER BY CASE scope WHEN 'global' THEN 0 ELSE 1 END, created_ms",
            )?;
            let rows = stmt.query_map(rusqlite::params![KIND_USER_RULE, project_root], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}
