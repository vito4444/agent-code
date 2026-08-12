use super::*;
use tempfile::TempDir;
use wkbd_proto::{EventPayload, SegmentId, SegmentKind, StopReason};

fn open(dir: &TempDir) -> Store {
    let opened = Store::open(dir.path()).expect("open");
    assert!(opened.degraded.is_none(), "fresh database must migrate cleanly");
    opened.store
}

fn chunk(text: &str) -> EventPayload {
    EventPayload::SegmentChunk {
        segment: SegmentId::from_agent("m1"),
        text: text.to_string(),
    }
}

#[tokio::test]
async fn sequence_numbers_come_from_the_database_and_survive_reopen() {
    let dir = TempDir::new().unwrap();

    let first_max = {
        let store = open(&dir);
        store
            .append(vec![
                wkbd_proto::PendingEvent::new("s1", chunk("a")),
                wkbd_proto::PendingEvent::new("s1", chunk("b")),
            ])
            .await
            .unwrap();
        store.high_water_mark().unwrap()
    };
    assert_eq!(first_max, 2);

    // Reopening simulates a restart. An in-memory counter would begin again at 1 here and
    // the next insert would collide with an existing primary key, which is the failure
    // that makes an application permanently unopenable.
    let store = open(&dir);
    let out = store
        .append(vec![wkbd_proto::PendingEvent::new("s1", chunk("c"))])
        .await
        .unwrap();
    assert_eq!(out[0].seq, 3, "numbering must continue from what is on disk");
}

#[tokio::test]
async fn sequence_numbers_are_never_reused_after_deletion() {
    // AUTOINCREMENT rather than a bare rowid: without it SQLite hands out the ids of
    // deleted rows again, and a reconnecting client that asked for "everything after 7"
    // would be told about a different event 8 than the one it missed.
    let dir = TempDir::new().unwrap();
    let store = open(&dir);

    store
        .append(vec![
            wkbd_proto::PendingEvent::new("s1", chunk("a")),
            wkbd_proto::PendingEvent::new("s1", chunk("b")),
        ])
        .await
        .unwrap();

    store
        .write(|tx| {
            tx.execute("DELETE FROM events WHERE seq = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();

    let out = store
        .append(vec![wkbd_proto::PendingEvent::new("s1", chunk("c"))])
        .await
        .unwrap();
    assert_eq!(out[0].seq, 3, "a deleted sequence number must not be handed out again");
}

#[tokio::test]
async fn read_since_returns_a_high_water_mark_that_tolerates_gaps() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);

    for i in 0..5 {
        store
            .append(vec![wkbd_proto::PendingEvent::new("s1", chunk(&format!("{i}")))])
            .await
            .unwrap();
    }
    // Punch a hole, as a rolled-back insert would.
    store
        .write(|tx| {
            tx.execute("DELETE FROM events WHERE seq = 3", [])?;
            Ok(())
        })
        .await
        .unwrap();

    let (events, hwm) = store.read_since(Some("s1"), 0, 100).unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(hwm, 5);
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 4, 5], "the gap is real and must not be papered over");

    // A client resuming from the mark gets nothing new, and must not conclude from the
    // missing 3 that it lost an event.
    let (tail, _) = store.read_since(Some("s1"), hwm, 100).unwrap();
    assert!(tail.is_empty());
}

#[tokio::test]
async fn events_are_scoped_per_session_but_numbered_globally() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);

    store.append_one("a", chunk("1")).await.unwrap();
    store.append_one("b", chunk("2")).await.unwrap();
    store.append_one("a", chunk("3")).await.unwrap();

    let (only_a, _) = store.read_since(Some("a"), 0, 100).unwrap();
    assert_eq!(only_a.len(), 2);
    assert_eq!(only_a[0].seq, 1);
    assert_eq!(only_a[1].seq, 3, "global numbering, so session a skips 2");

    let (all, _) = store.read_since(None, 0, 100).unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn payloads_round_trip_through_json() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);

    let original = EventPayload::TurnEnded { turn: 4, stop_reason: StopReason::Refusal };
    store.append_one("s", original.clone()).await.unwrap();
    let (events, _) = store.read_since(Some("s"), 0, 10).unwrap();
    assert_eq!(events[0].payload, original);

    let seg = EventPayload::SegmentStarted {
        segment: SegmentId::synthesized(2, 1),
        kind: SegmentKind::Thought,
    };
    store.append_one("s", seg.clone()).await.unwrap();
    let (events, _) = store.read_since(Some("s"), 1, 10).unwrap();
    assert_eq!(events[0].payload, seg);
}

#[tokio::test]
async fn concurrent_appends_all_get_distinct_sequence_numbers() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);

    let mut handles = Vec::new();
    for agent in 0..10 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..20 {
                s.append_one(format!("agent{agent}"), chunk(&format!("{i}")))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let (all, hwm) = store.read_since(None, 0, 1000).unwrap();
    assert_eq!(all.len(), 200);
    assert_eq!(hwm, 200);
    let mut seqs: Vec<i64> = all.iter().map(|e| e.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 200, "no duplicate sequence numbers under concurrency");
}

#[tokio::test]
async fn blobs_are_content_addressed_and_deduplicated() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let blobs = store.blobs();

    let h1 = blobs.put(b"a large diff").unwrap();
    let h2 = blobs.put(b"a large diff").unwrap();
    assert_eq!(h1, h2);
    assert_eq!(blobs.get(&h1).unwrap(), b"a large diff");
    assert!(blobs.exists(&h1));
    assert!(!blobs.exists(&"0".repeat(64)));
}

#[test]
fn migrations_are_ordered_and_idempotent() {
    let dir = TempDir::new().unwrap();
    {
        let opened = Store::open(dir.path()).unwrap();
        assert!(opened.degraded.is_none());
    }
    // Reopening must be a no-op, not a re-application.
    let opened = Store::open(dir.path()).unwrap();
    assert!(opened.degraded.is_none());

    let conn = opened.store.read_conn().unwrap();
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, schema::latest_version());

    let versions: Vec<i64> = schema::MIGRATIONS.iter().map(|m| m.version).collect();
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(versions, sorted, "migration versions must be unique and ascending");
}

#[test]
fn a_failed_migration_degrades_to_read_only_rather_than_refusing_to_start() {
    let dir = TempDir::new().unwrap();
    {
        // Create a database at version 1 with a table that migration 2 will collide with,
        // so applying the rest fails the way a corrupt or hand-edited database would.
        let mut conn = rusqlite::Connection::open(dir.path().join("workbench.db")).unwrap();
        conn.execute_batch(schema::MIGRATIONS[0].sql).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute_batch("CREATE TABLE facts (nonsense INTEGER);").unwrap();
    }

    let opened = Store::open(dir.path()).expect("must still open");
    assert!(
        opened.degraded.is_some(),
        "the caller needs a reason to show the user, not a silent success"
    );
    assert!(opened.store.is_read_only());

    // Reads still work, so the user can see and export their data.
    let (events, _) = opened.store.read_since(None, 0, 10).unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn writes_are_refused_while_degraded_instead_of_corrupting_further() {
    let dir = TempDir::new().unwrap();
    {
        let mut conn = rusqlite::Connection::open(dir.path().join("workbench.db")).unwrap();
        conn.execute_batch(schema::MIGRATIONS[0].sql).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute_batch("CREATE TABLE facts (nonsense INTEGER);").unwrap();
    }
    let opened = Store::open(dir.path()).unwrap();
    let err = opened.store.append_one("s", chunk("x")).await.unwrap_err();
    assert!(format!("{err}").contains("read-only"));
}

#[test]
fn a_backup_is_written_before_a_schema_change() {
    let dir = TempDir::new().unwrap();
    {
        let mut conn = rusqlite::Connection::open(dir.path().join("workbench.db")).unwrap();
        conn.execute_batch(schema::MIGRATIONS[0].sql).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }
    let opened = Store::open(dir.path()).unwrap();
    assert!(opened.degraded.is_none());

    let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
        .expect("a backup directory must exist once a migration has run")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(backups.len(), 1, "exactly one backup for one migration run");
    assert!(backups[0].path().to_string_lossy().contains("workbench-v1-"));
}

#[test]
fn boot_guard_escalates_then_clears() {
    let dir = TempDir::new().unwrap();

    // First launch: nothing known, normal mode.
    let (mut g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::Off);
    assert!(!out.previous_launch_failed);
    g.mark_healthy();

    // A launch that never reaches healthy leaves the counter raised.
    let (_g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::Off);
    drop(_g);

    let (_g, out) = BootGuard::begin(dir.path()).unwrap();
    assert!(out.previous_launch_failed);
    assert_eq!(out.consecutive_failures, 2);
    drop(_g);

    // Third consecutive failure: stop restoring session state.
    let (_g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::SkipRestore);
    assert!(!out.safe_mode.restores_sessions());
    assert!(out.safe_mode.starts_agents());
    drop(_g);

    let (_g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::SkipRestore);
    drop(_g);

    // Fifth: also stop launching agents.
    let (mut g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::Minimal);
    assert!(!out.safe_mode.starts_agents());

    // One healthy launch resets everything.
    g.mark_healthy();
    let (_g, out) = BootGuard::begin(dir.path()).unwrap();
    assert_eq!(out.safe_mode, SafeMode::Off);
    assert_eq!(out.consecutive_failures, 1);
}

#[test]
fn boot_state_lives_outside_the_main_database() {
    // If the counter lived in workbench.db then a corrupt workbench.db would take the
    // counter with it, and the escalation that exists to rescue the user would be erased
    // by the very corruption it is meant to detect.
    let dir = TempDir::new().unwrap();
    let (mut g, _) = BootGuard::begin(dir.path()).unwrap();
    g.mark_healthy();

    assert!(dir.path().join("boot-state.json").exists());
    let opened = Store::open(dir.path()).unwrap();
    let conn = opened.store.read_conn().unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE '%boot%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}
