//! Retrieval.
//!
//! Two FTS indexes, chosen by what the query contains. `unicode61` splits on Unicode word
//! boundaries, which for a script that does not put spaces between words turns a whole
//! sentence into one token: the search returns nothing, reports no error, and the corpus
//! silently becomes unsearchable. `trigram` handles that and arbitrary substrings, but its
//! index is larger and it degrades below three characters, so Latin text and code identifiers
//! still go through `unicode61` where BM25 ranking behaves as intended.
//!
//! Results from both are combined with reciprocal rank fusion. Fusing ranks rather than scores
//! avoids having to make two different scoring scales commensurable, which they are not.

use anyhow::Result;
use wkbd_store::Store;

use crate::facts::{Fact, SourceTrust};

/// Ranked recall for injection into a session prelude.
pub struct Recalled {
    pub fact: Fact,
    pub rank: f64,
}

fn has_cjk(query: &str) -> bool {
    query.chars().any(|c| {
        matches!(c as u32,
            0x4E00..=0x9FFF   // CJK unified ideographs
            | 0x3400..=0x4DBF // extension A
            | 0x3040..=0x30FF // hiragana and katakana
            | 0xAC00..=0xD7AF // hangul syllables
        )
    })
}

/// Turns user text into an FTS5 MATCH expression.
///
/// Every token is quoted. FTS5 treats bare `-`, `*`, `:` and `^` as operators, so an
/// unquoted query containing a path or a flag is either a syntax error or silently means
/// something else.
fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Searches one FTS table and returns fact ids in rank order.
async fn search_table(
    store: &Store,
    table: &'static str,
    query: String,
    project_root: Option<String>,
    limit: usize,
) -> Result<Vec<String>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = fts_query(&query);
    store
        .read(move |conn| {
            let sql = format!(
                "SELECT f.id FROM {table} t
                 JOIN facts f ON f.rowid = t.rowid
                 WHERE t.{table} MATCH ?1
                   AND f.kind = 'inferred'
                   AND f.expired_ms IS NULL
                   AND (f.project_root IS ?2 OR f.project_root IS NULL)
                 ORDER BY bm25({table})
                 LIMIT ?3"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                rusqlite::params![match_expr, project_root, limit as i64],
                |row| row.get::<_, String>(0),
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Recalls facts relevant to a query.
pub async fn search(
    store: &Store,
    query: &str,
    project_root: Option<&str>,
    limit: usize,
) -> Result<Vec<Recalled>> {
    let root = project_root.map(str::to_string);

    // Very short CJK queries would fall below the trigram window, so they go to LIKE instead.
    // At the scale a single user's memory reaches, a scan is still sub-millisecond, and a
    // correct answer from a slow path beats an empty answer from a fast one.
    let trimmed = query.trim();
    let short_cjk = has_cjk(trimmed) && trimmed.chars().count() < 3;

    let mut ranked: Vec<Vec<String>> = Vec::new();

    if short_cjk {
        ranked.push(like_search(store, trimmed.to_string(), root.clone(), limit).await?);
    } else {
        ranked.push(
            search_table(store, "facts_fts", query.to_string(), root.clone(), limit).await?,
        );
        if has_cjk(trimmed) || trimmed.contains('_') || trimmed.contains("::") {
            ranked.push(
                search_table(store, "facts_fts_tri", query.to_string(), root.clone(), limit)
                    .await?,
            );
        }
    }

    // Reciprocal rank fusion. k=60 is the conventional constant; its only job is to stop the
    // top result of one list from dominating everything the other list found.
    const K: f64 = 60.0;
    let mut fused: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for list in &ranked {
        for (i, id) in list.iter().enumerate() {
            *fused.entry(id.clone()).or_insert(0.0) += 1.0 / (K + (i as f64) + 1.0);
        }
    }

    let mut ids: Vec<(String, f64)> = fused.into_iter().collect();
    ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ids.truncate(limit);

    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let wanted: Vec<String> = ids.iter().map(|(id, _)| id.clone()).collect();
    let facts = load_facts(store, wanted).await?;

    let mut out = Vec::new();
    for (id, rank) in ids {
        if let Some(fact) = facts.iter().find(|f| f.id == id) {
            out.push(Recalled { fact: fact.clone(), rank });
        }
    }
    Ok(out)
}

async fn like_search(
    store: &Store,
    needle: String,
    project_root: Option<String>,
    limit: usize,
) -> Result<Vec<String>> {
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM facts
                 WHERE kind = 'inferred' AND expired_ms IS NULL
                   AND (project_root IS ?2 OR project_root IS NULL)
                   AND body LIKE '%' || ?1 || '%'
                 ORDER BY created_ms DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(rusqlite::params![needle, project_root, limit as i64], |r| {
                r.get::<_, String>(0)
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Loads facts by id.
///
/// Deliberately not filtered by project: the search that produced these ids already applied
/// the project filter, and re-applying it here with a different predicate is how a match gets
/// found and then silently discarded.
async fn load_facts(store: &Store, ids: Vec<String>) -> Result<Vec<Fact>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    store
        .read(move |conn| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, scope, project_root, subject, predicate, body, confidence,
                        valid_at, invalid_at, created_ms, expired_ms, source_run,
                        source_trust, superseded_by
                 FROM facts WHERE kind = 'inferred' AND id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), crate::facts::row_to_fact_pub)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
}

/// Renders a recalled fact for the prelude.
///
/// Provenance is included deliberately. A memory is evidence, and evidence the reader cannot
/// trace is evidence they cannot discount. The confidence and the source appear inline so the
/// model — and a human reading the transcript — can see that this is an inference rather than
/// an instruction.
pub fn render_for_prelude(recalled: &Recalled) -> String {
    let fact = &recalled.fact;
    let mut parts = Vec::new();
    if let Some(c) = fact.confidence {
        parts.push(format!("confidence {c:.2}"));
    }
    if let Some(run) = &fact.source_run {
        parts.push(format!("from run {run}"));
    }
    if fact.source_trust == SourceTrust::External {
        parts.push("source outside this project".to_string());
    }
    if parts.is_empty() {
        fact.body.clone()
    } else {
        format!("[{}] {}", parts.join(" | "), fact.body)
    }
}
