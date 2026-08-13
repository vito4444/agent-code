//! Schema migrations.
//!
//! Three rules, all of which come from this being a desktop application rather than a
//! service:
//!
//! - **Forward only.** A user will not read release notes and will not be given a
//!   rollback window. Down migrations that are never exercised are worse than no down
//!   migrations, because they look like a safety net.
//! - **Back up before migrating.** `VACUUM INTO` produces a consistent copy of a live
//!   database without stopping writers, which is both safer than copying the file and
//!   simpler than the backup API.
//! - **A failed migration degrades to read-only; it never prevents startup.** Anything on
//!   the startup path that can hard-fail eventually will, and an application that cannot
//!   be opened cannot be used to fix itself.

use rusqlite::Connection;

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "event_log",
        sql: r#"
-- The append-only event log. Every derived view in the system is a fold over this
-- table, and nothing mutates state except by appending here.
--
-- `seq` is INTEGER PRIMARY KEY AUTOINCREMENT rather than a plain rowid on purpose.
-- Without AUTOINCREMENT, SQLite reuses the ids of deleted rows, and a reused id in an
-- append-only log means a client that reconnects with `since_seq` can silently miss or
-- duplicate events. AUTOINCREMENT guarantees "larger than any id that has ever existed"
-- at the cost of one extra row update in sqlite_sequence per insert.
--
-- It guarantees monotonicity, NOT contiguity: a failed insert leaves a permanent gap.
-- Consumers must therefore track a high-water mark and must never infer loss from a gap.
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT    NOT NULL,
    at_ms      INTEGER NOT NULL,
    payload    TEXT    NOT NULL
);
CREATE INDEX idx_events_session_seq ON events(session_id, seq);

CREATE TABLE sessions (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT    NOT NULL,
    project_root       TEXT    NOT NULL,
    created_ms         INTEGER NOT NULL,
    closed_ms          INTEGER,
    -- The agent's own session id, needed for session/load on reconnect. Null until the
    -- agent has told us one.
    acp_session_id     TEXT,
    -- Identifies the process-pool slot this session belongs to: agent plus every setting
    -- that can only be applied at spawn time. Sessions with different fingerprints can
    -- never share a process.
    config_fingerprint TEXT    NOT NULL,
    title              TEXT
);
CREATE INDEX idx_sessions_project ON sessions(project_root);

-- Large payloads (whole diffs, terminal transcripts, raw model responses) live on disk
-- under their content hash; the database keeps only the pointer. Keeps the database
-- small enough that VACUUM INTO before a migration stays fast.
CREATE TABLE blobs (
    hash       TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    created_ms INTEGER NOT NULL
);
"#,
    },
    Migration {
        version: 2,
        name: "memory_and_rules",
        sql: r#"
-- Memory and user rules share a table but are separated by `kind`, and that separation
-- is enforced in three places on purpose (see wkbd-memory): the extractor's output schema
-- has no name for the user-rule kind, this CHECK constraint rejects it at the storage
-- layer, and every consolidation query filters it out. A preference that could be
-- rewritten by the model or retired by confidence decay is a setting the program has
-- quietly stopped honouring while still displaying it as enabled.
CREATE TABLE facts (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('user_rule', 'inferred')),
    scope         TEXT NOT NULL CHECK (scope IN ('global', 'project')),
    project_root  TEXT,
    subject       TEXT NOT NULL,
    predicate     TEXT NOT NULL,
    body          TEXT NOT NULL,
    confidence    REAL,

    -- Event time: when the statement became / stopped being true in the world.
    valid_at      INTEGER,
    invalid_at    INTEGER,
    -- System time: when we recorded it, and when we stopped believing it.
    created_ms    INTEGER NOT NULL,
    expired_ms    INTEGER,

    -- Which run taught us this, and how much the source is trusted. Content that came
    -- from outside the project (issue text, third-party repos, fetched pages) must never
    -- be promoted to behaviour-affecting on its own.
    source_run    TEXT,
    source_trust  TEXT NOT NULL DEFAULT 'internal'
                    CHECK (source_trust IN ('user', 'internal', 'external')),
    superseded_by TEXT,
    enabled       INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX idx_facts_live ON facts(kind, scope, project_root, expired_ms);
CREATE INDEX idx_facts_group ON facts(subject, predicate);

-- Two FTS tables, not one. unicode61 splits on Unicode word boundaries, which for text
-- with no spaces between words turns an entire Chinese sentence into a single token: the
-- search returns nothing and reports no error, so the failure is invisible until someone
-- notices half the corpus is unreachable. trigram handles CJK and arbitrary substrings
-- but is larger and degrades below three characters, so Latin text and code identifiers
-- still go through unicode61 where BM25 ranking behaves.
CREATE VIRTUAL TABLE facts_fts USING fts5(
    body,
    content='facts',
    content_rowid='rowid',
    tokenize='unicode61'
);
CREATE VIRTUAL TABLE facts_fts_tri USING fts5(
    body,
    content='facts',
    content_rowid='rowid',
    tokenize='trigram'
);

-- External-content FTS tables do not update themselves. Every one of these triggers is
-- required; a missing one produces a silently stale index, which is the same failure
-- shape as the tokenizer problem above.
CREATE TRIGGER facts_ai AFTER INSERT ON facts BEGIN
    INSERT INTO facts_fts(rowid, body) VALUES (new.rowid, new.body);
    INSERT INTO facts_fts_tri(rowid, body) VALUES (new.rowid, new.body);
END;
CREATE TRIGGER facts_ad AFTER DELETE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, body) VALUES('delete', old.rowid, old.body);
    INSERT INTO facts_fts_tri(facts_fts_tri, rowid, body) VALUES('delete', old.rowid, old.body);
END;
CREATE TRIGGER facts_au AFTER UPDATE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, body) VALUES('delete', old.rowid, old.body);
    INSERT INTO facts_fts_tri(facts_fts_tri, rowid, body) VALUES('delete', old.rowid, old.body);
    INSERT INTO facts_fts(rowid, body) VALUES (new.rowid, new.body);
    INSERT INTO facts_fts_tri(rowid, body) VALUES (new.rowid, new.body);
END;
"#,
    },
    Migration {
        version: 3,
        name: "orchestration",
        sql: r#"
-- Durable execution, shaped after DBOS rather than Temporal: no separate orchestration
-- server, just a status row plus a checkpoint row per completed step, and a scan for
-- unfinished work at startup. The rule that makes it work is that a step which has been
-- checkpointed is never re-executed.
CREATE TABLE runs (
    id           TEXT PRIMARY KEY,
    goal         TEXT NOT NULL,
    project_root TEXT NOT NULL,
    status       TEXT NOT NULL
                   CHECK (status IN ('planning','running','blocked','done','failed','cancelled')),
    created_ms   INTEGER NOT NULL,
    updated_ms   INTEGER NOT NULL,
    base_commit  TEXT
);

CREATE TABLE tasks (
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES runs(id),
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    status        TEXT NOT NULL
                    CHECK (status IN ('pending','ready','dispatched','verifying','completed','failed','blocked')),
    -- Declared file ownership. A scheduling heuristic only: measured on this machine,
    -- two branches touching disjoint paths can still fail to merge (directory rename
    -- split), so merge-tree is the authority and this is just a cheap pre-filter.
    declared_paths TEXT NOT NULL,
    -- Machine-checkable acceptance, not prose. Free-text "test strategy" fields are why
    -- every existing tool stops at human review.
    verify_spec   TEXT NOT NULL,
    attempt       INTEGER NOT NULL DEFAULT 0,
    branch        TEXT,
    worktree_path TEXT,
    start_commit  TEXT,
    end_commit    TEXT,
    created_ms    INTEGER NOT NULL,
    updated_ms    INTEGER NOT NULL
);
CREATE INDEX idx_tasks_run ON tasks(run_id, status);

CREATE TABLE task_deps (
    task_id    TEXT NOT NULL REFERENCES tasks(id),
    depends_on TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);

-- One row per completed step. Presence of a row is what makes a step never run twice.
CREATE TABLE step_outputs (
    run_id     TEXT NOT NULL REFERENCES runs(id),
    step_key   TEXT NOT NULL,
    output     TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (run_id, step_key)
);

CREATE TABLE verifications (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    attempt     INTEGER NOT NULL,
    passed      INTEGER NOT NULL,
    -- Hash of the patch under test. The cache key must include the content being
    -- verified: a harness keyed only on run id will happily replay a previous verdict
    -- for a different patch.
    patch_hash  TEXT NOT NULL,
    report      TEXT NOT NULL,
    created_ms  INTEGER NOT NULL
);
CREATE INDEX idx_verifications_task ON verifications(task_id, attempt);
"#,
    },
    Migration {
        version: 4,
        name: "evolution",
        sql: r#"
-- Working notes as itemized bullets with helpful/harmful counters, merged by ordinary
-- code. The alternative, letting a model rewrite the whole document each round, has a
-- measured failure mode: the context collapses to a short summary and accuracy drops
-- below the no-adaptation baseline.
CREATE TABLE playbook (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,
    body          TEXT NOT NULL,
    helpful       INTEGER NOT NULL DEFAULT 0,
    harmful       INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active','deprecated')),
    source_trust  TEXT NOT NULL DEFAULT 'internal'
                    CHECK (source_trust IN ('user','internal','external')),
    source_run    TEXT,
    -- Bullets expire. Pruning only by harmful count has no time dimension, so a note
    -- that was right about a codebase that has since changed never ages out.
    expires_ms    INTEGER,
    created_ms    INTEGER NOT NULL,
    updated_ms    INTEGER NOT NULL
);
CREATE INDEX idx_playbook_scope ON playbook(scope, status);

-- Anything the system writes for its own future consumption arrives here first and does
-- nothing until approved. Approval binds to the hash of the exact bytes: binding it to a
-- name or an id is the mistake behind CVE-2025-54136, where an approved entry could be
-- swapped for a different payload without re-prompting.
CREATE TABLE proposals (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    scope         TEXT NOT NULL,
    body          TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    evidence      TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','approved','rejected','superseded','expired')),
    risk          TEXT NOT NULL DEFAULT 'normal'
                    CHECK (risk IN ('normal','elevated')),
    created_ms    INTEGER NOT NULL,
    decided_ms    INTEGER,
    -- The hash that was actually shown to the human. If it differs from content_hash at
    -- apply time the approval is void.
    approved_hash TEXT
);
CREATE INDEX idx_proposals_status ON proposals(status, created_ms);

-- Contextual bandit state for routing. Rewards arrive hours or days late (was the pull
-- request merged? was it reverted?), so the feature vector is stored at decision time
-- rather than recomputed when the reward lands.
CREATE TABLE routing_decisions (
    id          TEXT PRIMARY KEY,
    arm         TEXT NOT NULL,
    role        TEXT NOT NULL,
    features    TEXT NOT NULL,
    cost        REAL,
    reward      REAL,
    rewarded_ms INTEGER,
    created_ms  INTEGER NOT NULL
);
CREATE INDEX idx_routing_pending ON routing_decisions(reward, created_ms);

CREATE TABLE routing_arms (
    arm         TEXT NOT NULL,
    role        TEXT NOT NULL,
    -- Serialized A matrix and b vector for LinUCB, plus the last update time so
    -- staleness-based variance inflation can be applied.
    a_matrix    TEXT NOT NULL,
    b_vector    TEXT NOT NULL,
    updates     INTEGER NOT NULL DEFAULT 0,
    updated_ms  INTEGER NOT NULL,
    PRIMARY KEY (arm, role)
);
"#,
    },
    Migration {
        version: 5,
        name: "proposal_terminal_states",
        sql: r#"
-- Separates "this ran" from "the approval was voided because the content changed".
--
-- Both previously retired to `superseded`, which folded a security-relevant event into a
-- housekeeping one: a proposal whose bytes changed after approval is the exact shape of a
-- known vulnerability, and it has to be visible as itself rather than as "something newer
-- came along". Recreating the table is the only way to widen a CHECK constraint in SQLite.
CREATE TABLE proposals_new (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    scope         TEXT NOT NULL,
    body          TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    evidence      TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','approved','rejected','superseded',
                                      'expired','applied','voided')),
    risk          TEXT NOT NULL DEFAULT 'normal'
                    CHECK (risk IN ('normal','elevated')),
    created_ms    INTEGER NOT NULL,
    decided_ms    INTEGER,
    approved_hash TEXT,
    -- When the payload actually took effect. Null for everything that never ran.
    applied_ms    INTEGER
);

INSERT INTO proposals_new
    (id, kind, scope, body, content_hash, evidence, status, risk,
     created_ms, decided_ms, approved_hash, applied_ms)
SELECT id, kind, scope, body, content_hash, evidence, status, risk,
       created_ms, decided_ms, approved_hash, NULL
FROM proposals;

DROP TABLE proposals;
ALTER TABLE proposals_new RENAME TO proposals;
CREATE INDEX idx_proposals_status ON proposals(status, created_ms);
"#,
    },
    Migration {
        version: 6,
        name: "run_awaiting_merge_state",
        sql: r#"
-- Separates "waiting for a human to merge" from "blocked".
--
-- Both would otherwise be `blocked`, and they need opposite handling: a blocked run has a
-- dependency that failed and there is nothing for anyone to do about it, while a run awaiting a
-- merge finished successfully and is waiting on a decision. Recovery treats them differently too —
-- a blocked run is over, an awaiting one must not be restarted, and a resume that cannot tell them
-- apart will either re-run finished work or abandon a candidate somebody was about to accept.
-- Recreating the table is the only way to widen a CHECK constraint in SQLite.
CREATE TABLE runs_new (
    id           TEXT PRIMARY KEY,
    goal         TEXT NOT NULL,
    project_root TEXT NOT NULL,
    status       TEXT NOT NULL
                   CHECK (status IN ('planning','running','blocked','awaiting_merge',
                                     'done','failed','cancelled')),
    created_ms   INTEGER NOT NULL,
    updated_ms   INTEGER NOT NULL,
    base_commit  TEXT
);

INSERT INTO runs_new (id, goal, project_root, status, created_ms, updated_ms, base_commit)
SELECT id, goal, project_root, status, created_ms, updated_ms, base_commit FROM runs;

DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;
"#,
    },
];

pub fn current_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

pub fn apply_pending(conn: &mut Connection) -> anyhow::Result<i64> {
    let mut version = current_version(conn)?;
    for m in MIGRATIONS {
        if m.version <= version {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)
            .map_err(|e| anyhow::anyhow!("migration {} ({}) failed: {e}", m.version, m.name))?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        tracing::info!(version = m.version, name = m.name, "applied migration");
        version = m.version;
    }
    Ok(version)
}

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}
