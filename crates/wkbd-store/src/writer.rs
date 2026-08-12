//! The single writer.
//!
//! Every write in the process funnels through one thread holding one connection. Two
//! reasons, in order of importance:
//!
//! 1. Sequence numbers must come from the database inside the write transaction. Funnel
//!    everything through one place and there is nowhere left to accidentally allocate one
//!    from memory.
//! 2. SQLite permits one writer at a time regardless. Ten connections contending produce
//!    lock thrash and `SQLITE_BUSY` retries; one queue produces batches.
//!
//! Appends are batched by size or by a short timer, whichever comes first, so a burst of
//! streaming chunks costs one fsync rather than one per chunk.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use wkbd_proto::{Event, PendingEvent};

#[derive(Clone, Debug)]
pub struct WriterConfig {
    pub max_batch: usize,
    pub linger: Duration,
}

impl Default for WriterConfig {
    fn default() -> Self {
        // 20ms is under one animation frame, so batching is invisible to the user, and at
        // realistic streaming rates it coalesces tens of chunks into one transaction.
        Self { max_batch: 64, linger: Duration::from_millis(20) }
    }
}

type ExecFn = Box<dyn FnOnce(&rusqlite::Transaction) -> Result<Box<dyn std::any::Any + Send>> + Send>;

enum Cmd {
    Append { events: Vec<PendingEvent>, reply: oneshot::Sender<Result<Vec<Event>>> },
    Exec { f: ExecFn, reply: oneshot::Sender<Result<Box<dyn std::any::Any + Send>>> },
}

#[derive(Clone)]
pub struct WriteHandle {
    tx: mpsc::Sender<Cmd>,
    read_only: bool,
}

impl WriteHandle {
    pub async fn append(&self, events: Vec<PendingEvent>) -> Result<Vec<Event>> {
        if self.read_only {
            anyhow::bail!("store is read-only after a failed migration; writes are refused");
        }
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Append { events, reply })
            .await
            .map_err(|_| anyhow::anyhow!("writer thread is gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("writer dropped the reply"))?
    }

    pub async fn exec<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Transaction) -> Result<T> + Send + 'static,
    {
        if self.read_only {
            anyhow::bail!("store is read-only after a failed migration; writes are refused");
        }
        let (reply, rx) = oneshot::channel();
        let boxed: ExecFn = Box::new(move |tx| {
            let v = f(tx)?;
            Ok(Box::new(v) as Box<dyn std::any::Any + Send>)
        });
        self.tx
            .send(Cmd::Exec { f: boxed, reply })
            .await
            .map_err(|_| anyhow::anyhow!("writer thread is gone"))?;
        let any = rx.await.map_err(|_| anyhow::anyhow!("writer dropped the reply"))??;
        any.downcast::<T>()
            .map(|b| *b)
            .map_err(|_| anyhow::anyhow!("writer returned an unexpected type"))
    }
}

pub fn spawn(db_path: PathBuf, cfg: WriterConfig, read_only: bool) -> Result<WriteHandle> {
    let (tx, mut rx) = mpsc::channel::<Cmd>(1024);

    std::thread::Builder::new()
        .name("wkbd-writer".into())
        .spawn(move || {
            let conn = match Connection::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "writer could not open the database");
                    return;
                }
            };
            if let Err(e) = super::tune(&conn) {
                tracing::error!(error = %e, "writer could not apply pragmas");
            }

            let rt = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "writer could not build its runtime");
                    return;
                }
            };

            rt.block_on(async move {
                let mut conn = conn;
                while let Some(first) = rx.recv().await {
                    match first {
                        Cmd::Exec { f, reply } => {
                            let r = run_exec(&mut conn, f);
                            let _ = reply.send(r);
                        }
                        Cmd::Append { events, reply } => {
                            // Gather whatever else is already queued, plus anything that
                            // shows up within the linger window, into one transaction.
                            let mut batch = vec![(events, reply)];
                            let deadline = tokio::time::Instant::now() + cfg.linger;
                            let mut queued: usize = batch[0].0.len();

                            while queued < cfg.max_batch {
                                let remaining = deadline.saturating_duration_since(
                                    tokio::time::Instant::now(),
                                );
                                if remaining.is_zero() {
                                    break;
                                }
                                match tokio::time::timeout(remaining, rx.recv()).await {
                                    Ok(Some(Cmd::Append { events, reply })) => {
                                        queued += events.len();
                                        batch.push((events, reply));
                                    }
                                    Ok(Some(Cmd::Exec { f, reply })) => {
                                        // Flush what we have first so ordering between an
                                        // append and a subsequent read-modify-write is
                                        // what the caller wrote.
                                        flush(&mut conn, std::mem::take(&mut batch));
                                        let r = run_exec(&mut conn, f);
                                        let _ = reply.send(r);
                                        break;
                                    }
                                    Ok(None) | Err(_) => break,
                                }
                            }
                            if !batch.is_empty() {
                                flush(&mut conn, batch);
                            }
                        }
                    }
                }
                tracing::debug!("writer thread exiting");
            });
        })
        .context("spawning writer thread")?;

    Ok(WriteHandle { tx, read_only })
}

fn run_exec(conn: &mut Connection, f: ExecFn) -> Result<Box<dyn std::any::Any + Send>> {
    // IMMEDIATE takes the write lock up front. A deferred transaction that reads first
    // and writes later can fail to upgrade and surface as SQLITE_BUSY under contention.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

type Batch = Vec<(Vec<PendingEvent>, oneshot::Sender<Result<Vec<Event>>>)>;

fn flush(conn: &mut Connection, batch: Batch) {
    if batch.is_empty() {
        return;
    }
    let at_ms = super::now_ms();

    let result: Result<Vec<Vec<Event>>> = (|| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut per_caller = Vec::with_capacity(batch.len());
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO events (session_id, at_ms, payload) VALUES (?1, ?2, ?3)",
            )?;
            for (events, _) in &batch {
                let mut written = Vec::with_capacity(events.len());
                for e in events {
                    let payload = serde_json::to_string(&e.payload)?;
                    stmt.execute(rusqlite::params![&e.session_id, at_ms, &payload])?;
                    // last_insert_rowid is the value AUTOINCREMENT just assigned, read
                    // back inside the same transaction. This is the only place a sequence
                    // number is ever produced.
                    let seq = tx.last_insert_rowid();
                    written.push(Event {
                        seq,
                        session_id: e.session_id.clone(),
                        at_ms,
                        payload: e.payload.clone(),
                    });
                }
                per_caller.push(written);
            }
        }
        tx.commit()?;
        Ok(per_caller)
    })();

    match result {
        Ok(per_caller) => {
            for ((_, reply), written) in batch.into_iter().zip(per_caller) {
                let _ = reply.send(Ok(written));
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            tracing::error!(error = %msg, "event batch failed");
            for (_, reply) in batch {
                let _ = reply.send(Err(anyhow::anyhow!(msg.clone())));
            }
        }
    }
}
