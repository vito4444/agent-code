//! Startup crash counter and safe mode.
//!
//! The startup path is where a desktop application dies permanently. It restores the
//! previous sessions, the previous worktrees, the previous event stream — all data that
//! a previous run wrote and that a bug in a previous run may have corrupted. If restoring
//! that data is what crashes, every subsequent launch crashes the same way and the user
//! has no way in.
//!
//! Two rules make this recoverable:
//!
//! - **The counter lives outside the main database.** If the thing that is corrupt is the
//!   database, and the counter lives in it, then the reset that is supposed to rescue the
//!   user also erases the evidence that a reset is needed.
//! - **Only things that can be rebuilt are discarded automatically.** Escalation stops at
//!   the point where the next step would destroy something the user cannot get back, and
//!   asks.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeMode {
    /// Normal start: restore everything.
    Off,
    /// Do not restore session state; start with an empty workbench but keep all data.
    /// Nothing is deleted, so this is always safe to enter automatically.
    SkipRestore,
    /// As above, and additionally refuse to auto-start any agent process.
    Minimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BootState {
    consecutive_failures: u32,
    last_start_ms: i64,
    last_success_ms: i64,
}

pub struct BootOutcome {
    pub safe_mode: SafeMode,
    pub consecutive_failures: u32,
    /// True when the previous launch never reached a usable state.
    pub previous_launch_failed: bool,
}

/// Tracks whether a launch reached a usable state.
///
/// The counter is incremented before any restore work happens and cleared only once the
/// daemon reports itself interactive. A process that dies in between therefore leaves the
/// counter raised, which is exactly the signal we want; success has to be affirmed, not
/// assumed.
pub struct BootGuard {
    path: PathBuf,
    state: BootState,
    marked_ok: bool,
}

impl BootGuard {
    pub fn begin(state_dir: &Path) -> Result<(Self, BootOutcome)> {
        std::fs::create_dir_all(state_dir)?;
        // Deliberately a separate small file, not a table in workbench.db.
        let path = state_dir.join("boot-state.json");

        let mut state: BootState = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => BootState::default(),
        };

        let previous_launch_failed = state.consecutive_failures > 0;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.last_start_ms = super::now_ms();

        let safe_mode = match state.consecutive_failures {
            0..=2 => SafeMode::Off,
            3..=4 => SafeMode::SkipRestore,
            _ => SafeMode::Minimal,
        };

        let guard = Self { path: path.clone(), state: state.clone(), marked_ok: false };
        // Best effort: if we cannot record the attempt we still start, we just lose the
        // ability to escalate. Refusing to start because the counter is unwritable would
        // be the failure this module exists to prevent.
        let _ = guard.persist();

        Ok((
            guard,
            BootOutcome {
                safe_mode,
                consecutive_failures: state.consecutive_failures,
                previous_launch_failed,
            },
        ))
    }

    /// Called once the daemon is serving. Clears the counter.
    pub fn mark_healthy(&mut self) {
        self.state.consecutive_failures = 0;
        self.state.last_success_ms = super::now_ms();
        self.marked_ok = true;
        if let Err(e) = self.persist() {
            tracing::warn!(error = %e, "could not clear the boot failure counter");
        }
    }

    pub fn was_marked_healthy(&self) -> bool {
        self.marked_ok
    }

    fn persist(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.state)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl SafeMode {
    pub fn restores_sessions(self) -> bool {
        matches!(self, SafeMode::Off)
    }

    pub fn starts_agents(self) -> bool {
        !matches!(self, SafeMode::Minimal)
    }
}
