//! Acceptance.
//!
//! Four layers, ordered by cost, and the cheap ones do most of the work. The problem they
//! address is measured rather than hypothetical: in an audit of a widely used benchmark, a
//! substantial fraction of tasks had test suites weak enough that a wrong patch passed, and a
//! short hook file placed where the test runner collects it is enough to report every test as
//! passing while changing nothing.
//!
//! 1. **The tests are read-only and their hashes are recorded** before the agent starts.
//! 2. **They are restored from the snapshot immediately before running**, so bypassing the file
//!    permissions gains nothing.
//! 3. **The verification command comes from the task graph**, not from the working tree.
//!    Otherwise editing the project's own test script is sufficient.
//! 4. **The result is reproduced from the start commit plus the recorded patch** in a clean
//!    location, which catches both a tampered tree and an accidental dependency on state the
//!    agent left behind.
//!
//! There is a fifth thing worth naming because it is easy to miss: the repository's own history
//! is readable from inside a worktree, and refs are shared across worktrees. If the answer
//! exists in a branch or in a reflog, an agent can find it without touching a test file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

use crate::graph::VerifySpec;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyOutcome {
    Passed,
    /// Assertions were evaluated and at least one failed.
    Failed { missing_pass: Vec<String>, regressed: Vec<String> },
    /// The setup commands failed, so no assertion was evaluated. Distinct from a test failure
    /// because the response differs: a broken environment is not the agent's mistake.
    SetupFailed { command: String, code: Option<i32>, output: String },
    /// The command ran but nothing could be concluded from its output.
    Inconclusive { reason: String },
    /// Files the agent was told not to touch were modified.
    Tampered { paths: Vec<String> },
}

impl VerifyOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, VerifyOutcome::Passed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub outcome: VerifyOutcome,
    /// Hash of the patch under test.
    ///
    /// Part of the cache key on purpose. A harness keyed only on a run identifier will happily
    /// return a previous verdict for a different patch, which is a real defect that shipped in
    /// the reference implementation of this kind of harness.
    pub patch_hash: String,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

pub fn hash_patch(patch: &str) -> String {
    let mut h = Sha256::new();
    h.update(patch.as_bytes());
    hex::encode(h.finalize())
}

/// Parses test identifiers out of runner output.
///
/// A trait because every runner formats results differently, and because the alternative —
/// deciding pass or fail from the exit code alone — cannot distinguish "the tests we care about
/// passed" from "the command exited zero", and those differ precisely when someone has
/// arranged for them to.
pub trait ResultParser: Send + Sync {
    fn passing_tests(&self, stdout: &str, stderr: &str) -> Option<Vec<String>>;
}

/// Reads the common `test <name> ... ok` shape emitted by Rust's test harness, and the
/// `test result:` summary line.
pub struct CargoTestParser;

impl ResultParser for CargoTestParser {
    fn passing_tests(&self, stdout: &str, _stderr: &str) -> Option<Vec<String>> {
        let mut passing = Vec::new();
        let mut saw_summary = false;
        for line in stdout.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                if let Some(name) = rest.strip_suffix(" ... ok") {
                    passing.push(name.trim().to_string());
                }
            }
            if line.starts_with("test result:") {
                saw_summary = true;
            }
        }
        // No summary line means the run did not complete, and an empty pass list from an
        // incomplete run must not be read as "these tests failed".
        if saw_summary {
            Some(passing)
        } else {
            None
        }
    }
}

pub struct Verifier<'a> {
    pub spec: &'a VerifySpec,
    pub parser: &'a dyn ResultParser,
    pub timeout: std::time::Duration,
}

impl<'a> Verifier<'a> {
    /// Runs the acceptance check in `workdir`.
    ///
    /// The caller is responsible for having restored immutable paths from the snapshot first;
    /// `check_tamper` reports whether they had been modified, so a tampering attempt is recorded
    /// even though it has already been undone.
    pub fn run(&self, workdir: &Path, patch: &str) -> Result<VerifyReport> {
        let started = std::time::Instant::now();
        let patch_hash = hash_patch(patch);

        for command in &self.spec.setup {
            let out = run_shell(workdir, command, self.timeout)?;
            if !out.status.success() {
                return Ok(VerifyReport {
                    outcome: VerifyOutcome::SetupFailed {
                        command: command.clone(),
                        code: out.status.code(),
                        output: format!(
                            "{}\n{}",
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr)
                        ),
                    },
                    patch_hash,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
        }

        let out = run_shell(workdir, &self.spec.cmd, self.timeout)?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        let outcome = match self.parser.passing_tests(&stdout, &stderr) {
            None => VerifyOutcome::Inconclusive {
                reason: "the runner did not produce a result summary, so nothing can be concluded"
                    .into(),
            },
            Some(passing) => {
                let missing_pass: Vec<String> = self
                    .spec
                    .must_pass
                    .iter()
                    .filter(|t| !passing.iter().any(|p| p == *t || p.ends_with(t.as_str())))
                    .cloned()
                    .collect();
                let regressed: Vec<String> = self
                    .spec
                    .must_still_pass
                    .iter()
                    .filter(|t| !passing.iter().any(|p| p == *t || p.ends_with(t.as_str())))
                    .cloned()
                    .collect();

                if missing_pass.is_empty() && regressed.is_empty() {
                    VerifyOutcome::Passed
                } else {
                    VerifyOutcome::Failed { missing_pass, regressed }
                }
            }
        };

        Ok(VerifyReport {
            outcome,
            patch_hash,
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn run_shell(
    workdir: &Path,
    command: &str,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    // A shell is used because acceptance commands are written by humans and contain pipes and
    // redirection. That is acceptable here and nowhere else in this codebase: the command comes
    // from the task graph, which the agent cannot write to.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning acceptance command: {command}"))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(_status) = child.try_wait()? {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("acceptance command timed out after {timeout:?}: {command}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(child.wait_with_output()?)
}
