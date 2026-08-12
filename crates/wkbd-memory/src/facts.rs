//! Inferred facts, with two time axes.
//!
//! A fact carries four timestamps:
//!
//! - `valid_at` / `invalid_at` — event time: when the statement was true in the world.
//! - `created_ms` / `expired_ms` — system time: when we recorded it, and when we stopped
//!   believing it.
//!
//! Nothing is ever deleted. When a new fact contradicts an old one, the old one gets an
//! `invalid_at` and an `expired_ms` and stays. That is what makes "what did we believe last
//! Wednesday" answerable, and deleting instead would make "we were wrong about this" and "we
//! never knew this" indistinguishable — which matters, because those two call for different
//! responses.
//!
//! The division of labour between the model and ordinary code is deliberate and narrow:
//!
//! - **The model decides only which existing facts a new one contradicts**, from a candidate
//!   set we choose.
//! - **All time arithmetic is code.** Interval endpoints are computed, not generated. A model
//!   inventing a plausible-looking date would corrupt the one axis the whole design rests on.
//! - **The candidate set is narrow.** Facts are grouped by subject and predicate first, and
//!   only same-group facts are offered for contradiction. A widely-scoped candidate pool plus
//!   a model asked to spot conflicts retires facts that are merely related, and the filtering
//!   has to happen before the model sees them rather than as a check on its answer.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wkbd_store::Store;

/// How much the source of a fact is trusted.
///
/// External content — issue text, a fetched page, a third-party repository's configuration —
/// can be written into memory by an agent that read it, and memory poisoning needs only one
/// successful write. External facts are recorded but never allowed to reach the prelude on
/// their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTrust {
    /// The user said it.
    User,
    /// Observed from our own run: a test result, a command we executed, a file we read in the
    /// project.
    Internal,
    /// Came from outside the project.
    External,
}

impl SourceTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceTrust::User => "user",
            SourceTrust::Internal => "internal",
            SourceTrust::External => "external",
        }
    }

    /// Whether a fact from this source may be injected without a human having approved it.
    pub fn may_auto_inject(self) -> bool {
        !matches!(self, SourceTrust::External)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fact {
    pub id: String,
    pub scope: String,
    pub project_root: Option<String>,
    /// Grouping key. Contradiction is only ever considered within one (subject, predicate).
    pub subject: String,
    pub predicate: String,
    pub body: String,
    pub confidence: Option<f64>,
    pub valid_at: Option<i64>,
    pub invalid_at: Option<i64>,
    pub created_ms: i64,
    pub expired_ms: Option<i64>,
    pub source_run: Option<String>,
    pub source_trust: SourceTrust,
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub project_root: Option<String>,
    pub subject: String,
    pub predicate: String,
    pub body: String,
    pub confidence: f64,
    /// When the statement became true, if known. Defaults to now.
    pub valid_at: Option<i64>,
    pub source_run: Option<String>,
    pub source_trust: SourceTrust,
}

/// What the model is allowed to decide.
///
/// Indices into the candidate list, and nothing else. No dates, no confidences, no new text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContradictionVerdict {
    pub contradicted: Vec<usize>,
}

/// Decides which candidates a new fact contradicts.
///
/// A trait so the memory layer does not depend on any particular model, and so the tests can
/// exercise the time arithmetic without one.
pub trait ContradictionJudge: Send + Sync {
    fn judge(&self, incoming: &NewFact, candidates: &[Fact]) -> ContradictionVerdict;
}

/// Treats an identical (subject, predicate) with different text as a contradiction.
///
/// Not a stand-in for a model: it is the behaviour we want when no model is available, and it
/// is deterministic, which makes it the right thing to run in tests and in replay.
pub struct SamePredicateJudge;

impl ContradictionJudge for SamePredicateJudge {
    fn judge(&self, incoming: &NewFact, candidates: &[Fact]) -> ContradictionVerdict {
        let contradicted = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.subject == incoming.subject
                    && c.predicate == incoming.predicate
                    && c.body.trim() != incoming.body.trim()
            })
            .map(|(i, _)| i)
            .collect();
        ContradictionVerdict { contradicted }
    }
}

pub struct RecordOutcome {
    pub id: String,
    /// Facts that were closed off because this one replaced them.
    pub superseded: Vec<String>,
}

/// Records a fact, closing off anything it contradicts.
pub async fn record(
    store: &Store,
    judge: &dyn ContradictionJudge,
    incoming: NewFact,
) -> Result<RecordOutcome> {
    let now = wkbd_store::now_ms();
    let valid_at = incoming.valid_at.unwrap_or(now);

    // The candidate set is limited to the same (subject, predicate) and the same project
    // before the judge sees anything. This is the guard, not a check on the answer.
    let candidates = live_in_group(store, &incoming).await?;

    let verdict = judge.judge(&incoming, &candidates);

    let id = uuid::Uuid::new_v4().to_string();
    let mut superseded = Vec::new();
    for index in &verdict.contradicted {
        match candidates.get(*index) {
            Some(fact) => {
                // Non-overlapping intervals are not contradictions. A fact that stopped being
                // true before this one started is history, not a conflict, and retiring it
                // would erase the record of a change rather than record it.
                if let Some(existing_invalid) = fact.invalid_at {
                    if existing_invalid <= valid_at {
                        continue;
                    }
                }
                superseded.push(fact.id.clone());
            }
            None => {
                // An index the judge invented. Ignored rather than trusted; a model that
                // hallucinates an index must not be able to retire an arbitrary fact.
                tracing::warn!(index, "contradiction verdict referenced a candidate that does not exist");
            }
        }
    }

    let insert_id = id.clone();
    let superseded_for_write = superseded.clone();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO facts
                 (id, kind, scope, project_root, subject, predicate, body, confidence,
                  valid_at, created_ms, source_run, source_trust, enabled)
                 VALUES (?1, 'inferred', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)",
                rusqlite::params![
                    insert_id,
                    if incoming.project_root.is_some() { "project" } else { "global" },
                    incoming.project_root,
                    incoming.subject,
                    incoming.predicate,
                    incoming.body,
                    incoming.confidence,
                    valid_at,
                    now,
                    incoming.source_run,
                    incoming.source_trust.as_str(),
                ],
            )?;

            for old in &superseded_for_write {
                // Event time closes at the moment the new fact became true; system time closes
                // now. Both are computed here, in code. Nothing about this is left to a model.
                tx.execute(
                    "UPDATE facts
                     SET invalid_at = ?2,
                         expired_ms = COALESCE(expired_ms, ?3),
                         superseded_by = ?4
                     WHERE id = ?1 AND kind = 'inferred'",
                    rusqlite::params![old, valid_at, now, insert_id],
                )?;
            }
            Ok(())
        })
        .await?;

    Ok(RecordOutcome { id, superseded })
}

async fn live_in_group(store: &Store, incoming: &NewFact) -> Result<Vec<Fact>> {
    let subject = incoming.subject.clone();
    let predicate = incoming.predicate.clone();
    let project_root = incoming.project_root.clone();
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, project_root, subject, predicate, body, confidence,
                        valid_at, invalid_at, created_ms, expired_ms, source_run,
                        source_trust, superseded_by
                 FROM facts
                 WHERE kind = 'inferred'
                   AND expired_ms IS NULL
                   AND subject = ?1 AND predicate = ?2
                   AND (project_root IS ?3 OR project_root IS NULL)
                 ORDER BY created_ms",
            )?;
            let rows = stmt.query_map(rusqlite::params![subject, predicate, project_root], row_to_fact)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Facts believed at a point in system time.
///
/// This is the query that pays for never deleting: "what did we think last Wednesday" is a
/// filter on `created_ms` and `expired_ms`, not an archaeology exercise.
pub async fn believed_at(
    store: &Store,
    project_root: Option<&str>,
    at_ms: i64,
) -> Result<Vec<Fact>> {
    let project_root = project_root.map(str::to_string);
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, project_root, subject, predicate, body, confidence,
                        valid_at, invalid_at, created_ms, expired_ms, source_run,
                        source_trust, superseded_by
                 FROM facts
                 WHERE kind = 'inferred'
                   AND created_ms <= ?1
                   AND (expired_ms IS NULL OR expired_ms > ?1)
                   AND (project_root IS ?2 OR project_root IS NULL)
                 ORDER BY created_ms",
            )?;
            let rows = stmt.query_map(rusqlite::params![at_ms, project_root], row_to_fact)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

pub async fn live(store: &Store, project_root: Option<&str>) -> Result<Vec<Fact>> {
    believed_at(store, project_root, wkbd_store::now_ms()).await
}

/// Re-exported for the recall module, which selects the same columns.
pub(crate) fn row_to_fact_pub(row: &rusqlite::Row) -> rusqlite::Result<Fact> {
    row_to_fact(row)
}

fn row_to_fact(row: &rusqlite::Row) -> rusqlite::Result<Fact> {
    let trust: String = row.get(12)?;
    Ok(Fact {
        id: row.get(0)?,
        scope: row.get(1)?,
        project_root: row.get(2)?,
        subject: row.get(3)?,
        predicate: row.get(4)?,
        body: row.get(5)?,
        confidence: row.get(6)?,
        valid_at: row.get(7)?,
        invalid_at: row.get(8)?,
        created_ms: row.get(9)?,
        expired_ms: row.get(10)?,
        source_run: row.get(11)?,
        source_trust: match trust.as_str() {
            "user" => SourceTrust::User,
            "external" => SourceTrust::External,
            _ => SourceTrust::Internal,
        },
        superseded_by: row.get(13)?,
    })
}
