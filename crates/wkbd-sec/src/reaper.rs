//! Three independent layers of subprocess cleanup.
//!
//! # Why three
//!
//! Leaked agent processes are the most common way this class of product fails in the
//! field. One shipped product's own telemetry over four days read `session-created 786`,
//! `session-kill-failed 171`, `session-closed 0` — nothing was ever cleaned up, and the
//! symptom that finally surfaced was hitting the pty limit. Another simply did not reap
//! the agent when its tab was closed.
//!
//! Each layer covers a failure the others cannot:
//!
//! 1. **Child side** — `prctl(PR_SET_PDEATHSIG, SIGKILL)`, so a child dies even if the
//!    daemon is `SIGKILL`ed and never gets to run any cleanup code. It only fires for the
//!    *immediate* parent, so a grandchild (agent → its own tool) is not covered;
//!    [`spawn_parent_watchdog`] is the fallback for a process started with something like
//!    `--watch-pid`.
//! 2. **Parent side** — every child gets its own process group, so termination is
//!    `killpg` and reaches the whole tree rather than just the process we happen to know
//!    about. [`install_child_subreaper`] additionally makes orphaned grandchildren
//!    reparent to the daemon instead of to pid 1, so they stay visible and reapable.
//! 3. **Boot side** — [`Supervisor::sweep_orphans`] cleans up what a hard crash left
//!    behind. **This must match on pid *and* identity.** Pids are reused; a registry that
//!    matches on pid alone eventually kills whatever unrelated process inherited that
//!    number, and that failure is far worse than the leak it was meant to fix.

// The registry and the identity checks are platform-independent and will be reused by the
// Windows implementation when it exists; until then nothing on Windows reaches them.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Exit status used by a child that detected its parent had already died before
/// `PR_SET_PDEATHSIG` could be installed.
pub const EXIT_PARENT_ALREADY_GONE: i32 = 121;

#[derive(Debug, thiserror::Error)]
pub enum ReaperError {
    #[error("failed to spawn supervised process: {0}")]
    Spawn(#[source] io::Error),

    #[error("registry {path}: {source}")]
    Registry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("registry {path} is not valid JSON: {source}")]
    RegistryFormat {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("process {pid} survived SIGKILL")]
    Unkillable { pid: u32 },

    #[error("{0}")]
    Io(#[source] io::Error),

    /// Windows needs a Job Object rather than a process group; see the comment on the
    /// Windows implementation below.
    #[error("not implemented on this platform: {0}")]
    Unimplemented(&'static str),
}

/// Enough about a process to tell it apart from an unrelated process that later reuses
/// its pid.
///
/// `start_ticks` is the clincher on Linux: the kernel's own start time for that pid, in
/// clock ticks since boot. A reused pid has a different start time, always.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// File name of the executable, not the full path: the path can move under a running
    /// process, the name in `/proc/<pid>/comm` cannot.
    pub exe_name: String,
    /// Wall-clock time we observed the process, for human-readable audit only. Never
    /// compared, because wall clocks move.
    pub started_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ticks: Option<u64>,
}

/// One live child, as recorded for the next boot to find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    #[serde(flatten)]
    pub identity: ProcessIdentity,
    /// Recorded so a sweep can kill the whole group. Equal to the pid, because every
    /// supervised child is made the leader of its own group.
    pub pgid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    entries: Vec<RegistryEntry>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        RegistryFile {
            version: 1,
            entries: Vec::new(),
        }
    }
}

/// What a boot-side sweep did, in enough detail to log per pid.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Entries read from the registry.
    pub examined: usize,
    /// Killed: pid present, identity matched.
    pub killed: Vec<u32>,
    /// **Deliberately left alone**: the pid exists but is now some other program. This
    /// counter going up is the pid-reuse guard doing its job.
    pub identity_mismatch: Vec<u32>,
    /// The pid no longer exists at all, which is the normal case after a clean shutdown.
    pub already_gone: Vec<u32>,
    /// Still there after `SIGKILL` (uninterruptible sleep, most likely), kept in the
    /// registry for the next boot.
    pub failed: Vec<u32>,
}

/// Outcome of terminating one child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// Was already dead when we asked.
    AlreadyExited,
    /// The whole group went away within the grace period after `SIGTERM`.
    ExitedOnSigterm,
    /// The grace period elapsed and `SIGKILL` finished the job.
    KilledOnSigkill,
}

struct Registry {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Spawns children so that they can always be cleaned up, and cleans up what a previous
/// run left behind.
#[derive(Clone, Default)]
pub struct Supervisor {
    registry: Option<Arc<Registry>>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("registry", &self.registry.as_ref().map(|r| &r.path))
            .finish()
    }
}

impl Supervisor {
    /// A supervisor with no registry: children are still grouped and still die with the
    /// daemon, but nothing survives a crash to be swept at the next boot.
    pub fn new() -> Self {
        Supervisor { registry: None }
    }

    /// The registry path is the caller's choice because only the caller knows where its
    /// state directory is, and because tests need to point it somewhere disposable.
    pub fn with_registry(path: impl Into<PathBuf>) -> Self {
        Supervisor {
            registry: Some(Arc::new(Registry {
                path: path.into(),
                lock: Mutex::new(()),
            })),
        }
    }

    pub fn registry_path(&self) -> Option<&Path> {
        self.registry.as_ref().map(|r| r.path.as_path())
    }

    /// Boot-side sweep of this supervisor's own registry.
    ///
    /// Call it before spawning anything: an orphan from the previous run is still holding
    /// the pty, the socket and the worktree lock that the new run is about to want.
    pub fn sweep_own_registry(&self) -> Result<SweepReport, ReaperError> {
        match self.registry_path() {
            Some(path) => Supervisor::sweep_orphans(path),
            None => Ok(SweepReport::default()),
        }
    }
}

// -----------------------------------------------------------------------------------
// Unix
// -----------------------------------------------------------------------------------

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// Makes orphaned grandchildren reparent to this process instead of to pid 1.
    ///
    /// Without it, an agent's own children survive the agent and become invisible: pid 1
    /// will reap them eventually, but nothing will ever *kill* them, and they are exactly
    /// the processes that hold ptys and sockets open.
    pub fn install_child_subreaper() -> Result<(), ReaperError> {
        #[cfg(target_os = "linux")]
        {
            let rc = unsafe {
                libc::prctl(
                    libc::PR_SET_CHILD_SUBREAPER,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            };
            if rc != 0 {
                return Err(ReaperError::Io(io::Error::last_os_error()));
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS has no equivalent; the process-group layer carries the weight there.
            Err(ReaperError::Unimplemented(
                "PR_SET_CHILD_SUBREAPER is Linux-only",
            ))
        }
    }

    impl Supervisor {
        /// Spawns `cmd` in its own process group, with the child configured to die if
        /// this process does.
        ///
        /// Stdio is left entirely to the caller: an ACP agent needs piped stdin/stdout,
        /// and this layer has no business overriding that.
        pub fn spawn_supervised(&self, mut cmd: Command) -> Result<SupervisedChild, ReaperError> {
            let exe_name = exe_name_of(&cmd);
            let parent_pid = std::process::id() as libc::pid_t;

            // Own process group, so termination is one killpg that reaches every
            // descendant that has not deliberately left the group.
            cmd.process_group(0);

            unsafe {
                cmd.pre_exec(move || {
                    // Layer 1. Covers the case where the daemon is SIGKILLed and never
                    // runs any cleanup at all. There is no macOS equivalent, which is why
                    // `spawn_parent_watchdog` exists as the portable substitute.
                    #[cfg(target_os = "linux")]
                    if libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGKILL as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    ) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    // The race PR_SET_PDEATHSIG cannot close by itself: if the parent
                    // died between fork and the prctl above, the signal was already not
                    // sent and never will be. Everything here must be async-signal-safe.
                    if libc::getppid() != parent_pid {
                        libc::_exit(EXIT_PARENT_ALREADY_GONE);
                    }
                    Ok(())
                });
            }

            let child = cmd.spawn().map_err(ReaperError::Spawn)?;
            let pid = child.id();
            // process_group(0) makes the child its own group leader, so pgid == pid. It
            // is recorded rather than recomputed because getpgid() on a process that has
            // already exited fails, and the sweep needs the number regardless.
            let pgid = pid;

            let identity = ProcessIdentity::of_pid(pid).unwrap_or_else(|| ProcessIdentity {
                pid,
                exe_name: exe_name.clone(),
                started_at_unix_ms: now_ms(),
                start_ticks: None,
            });

            if let Some(registry) = &self.registry {
                registry.insert(RegistryEntry {
                    identity: identity.clone(),
                    pgid,
                })?;
            }

            Ok(SupervisedChild {
                child: Some(child),
                identity,
                pgid,
                registry: self.registry.clone(),
            })
        }

        /// Boot-side cleanup of whatever a previous run left running.
        ///
        /// A missing registry is not an error: it is what a first run looks like.
        pub fn sweep_orphans(registry_path: &Path) -> Result<SweepReport, ReaperError> {
            let file = read_registry(registry_path)?;
            let mut report = SweepReport {
                examined: file.entries.len(),
                ..Default::default()
            };
            let mut survivors = Vec::new();

            for entry in file.entries {
                let pid = entry.identity.pid;
                match ProcessIdentity::of_pid(pid) {
                    None => report.already_gone.push(pid),
                    Some(current) => {
                        if !entry.identity.matches(&current) {
                            // The pid was recycled. Killing here is how a cleanup routine
                            // turns into an outage.
                            tracing::warn!(
                                pid,
                                recorded = %entry.identity.exe_name,
                                observed = %current.exe_name,
                                "registry pid was reused by another process; not killing"
                            );
                            report.identity_mismatch.push(pid);
                            continue;
                        }
                        // Killing the group is safe only because the identity of its
                        // leader was just verified: the group id equals that pid, so a
                        // group under this id cannot belong to anyone else.
                        match kill_group(entry.pgid, pid, Duration::from_millis(500)) {
                            Ok(()) => report.killed.push(pid),
                            Err(_) => {
                                report.failed.push(pid);
                                survivors.push(entry);
                            }
                        }
                    }
                }
            }

            write_registry(
                registry_path,
                &RegistryFile {
                    version: 1,
                    entries: survivors,
                },
            )?;
            Ok(report)
        }
    }

    /// A child that will not outlive the daemon.
    pub struct SupervisedChild {
        // Taken by `terminate`, so `Drop` can tell the two cases apart.
        pub(super) child: Option<Child>,
        pub(super) identity: ProcessIdentity,
        pub(super) pgid: u32,
        pub(super) registry: Option<Arc<Registry>>,
    }

    impl std::fmt::Debug for SupervisedChild {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SupervisedChild")
                .field("identity", &self.identity)
                .field("pgid", &self.pgid)
                .finish()
        }
    }

    impl SupervisedChild {
        pub fn pid(&self) -> u32 {
            self.identity.pid
        }

        pub fn pgid(&self) -> u32 {
            self.pgid
        }

        pub fn identity(&self) -> &ProcessIdentity {
            &self.identity
        }

        /// Access to the underlying handle, for stdio. Killing through it bypasses the
        /// group and leaves grandchildren behind, so do not.
        pub fn child_mut(&mut self) -> Option<&mut Child> {
            self.child.as_mut()
        }

        /// `SIGTERM` to the whole group, then `SIGKILL` to the whole group once `grace`
        /// has elapsed.
        ///
        /// Signalling the group rather than the pid is the entire point: a shell that
        /// forked a background job passes `SIGTERM` on to nobody, and killing only the
        /// pid we know leaves the job running.
        pub fn terminate(&mut self, grace: Duration) -> Result<Termination, ReaperError> {
            let mut child = match self.child.take() {
                Some(child) => child,
                None => return Ok(Termination::AlreadyExited),
            };
            let outcome = terminate_group(&mut child, self.pgid, grace);
            if let Some(registry) = &self.registry {
                // Best effort: a registry write failure must not mask the termination
                // result, and a stale entry is handled by the identity check at sweep.
                let _ = registry.remove(self.identity.pid);
            }
            outcome
        }

        /// True once the direct child has exited. Says nothing about grandchildren; use
        /// [`process_group_is_empty`] for that.
        pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            match &mut self.child {
                Some(child) => child.try_wait(),
                None => Ok(None),
            }
        }
    }

    impl Drop for SupervisedChild {
        fn drop(&mut self) {
            // No grace period here. Drop runs during unwinding, and a panicking daemon
            // that blocks for seconds per child is a daemon that gets SIGKILLed halfway
            // through cleaning up. The child-side PDEATHSIG covers the ordering risk.
            if let Some(mut child) = self.child.take() {
                let _ = signal_group(self.pgid, libc::SIGKILL);
                let _ = child.wait();
                if let Some(registry) = &self.registry {
                    let _ = registry.remove(self.identity.pid);
                }
            }
        }
    }

    fn terminate_group(
        child: &mut Child,
        pgid: u32,
        grace: Duration,
    ) -> Result<Termination, ReaperError> {
        let already_dead = matches!(child.try_wait(), Ok(Some(_)));

        if signal_group(pgid, libc::SIGTERM).is_err() && already_dead {
            return Ok(Termination::AlreadyExited);
        }

        if wait_for_group_exit(child, pgid, grace) {
            return Ok(if already_dead {
                Termination::AlreadyExited
            } else {
                Termination::ExitedOnSigterm
            });
        }

        signal_group(pgid, libc::SIGKILL).ok();
        // A process in uninterruptible sleep can outlast even SIGKILL for a while, so
        // this is a bounded wait rather than a blocking one; the caller gets an error
        // instead of a hang.
        if wait_for_group_exit(child, pgid, Duration::from_secs(5)) {
            Ok(Termination::KilledOnSigkill)
        } else {
            Err(ReaperError::Unkillable { pid: child.id() })
        }
    }

    /// Waits for the direct child *and* every other member of its process group.
    ///
    /// Waiting only for the child is the bug that leaves `sleep 300 &` running after the
    /// shell that started it has been reaped.
    fn wait_for_group_exit(child: &mut Child, pgid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // The direct child first, so its status is still available to the caller;
            // only then the reparented remainder of the group.
            let _ = child.try_wait();
            reap_group_zombies(pgid);
            if process_group_is_empty(pgid) {
                // The emptiness check skips zombies, and a group member can become one
                // while that check is walking /proc. Draining again closes that window:
                // if anything was still there to reap, the group was not clean and the
                // check has to run once more.
                if reap_group_zombies(pgid) {
                    continue;
                }
                let _ = child.wait();
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn signal_group(pgid: u32, signal: libc::c_int) -> io::Result<()> {
        // Guard against 0 and 1: killpg(0) signals *our own* group, which would take the
        // daemon down with it.
        if pgid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to signal process group 0 or 1",
            ));
        }
        if unsafe { libc::killpg(pgid as libc::pid_t, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sweep-side kill for a process that is not our child, so there is nothing to
    /// `wait` for; pid 1 (or a subreaper) does the reaping.
    fn kill_group(pgid: u32, pid: u32, grace: Duration) -> Result<(), ReaperError> {
        let _ = signal_group(pgid, libc::SIGTERM);
        let _ = kill_pid(pid, libc::SIGTERM);

        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !pid_is_running(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = signal_group(pgid, libc::SIGKILL);
        let _ = kill_pid(pid, libc::SIGKILL);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !pid_is_running(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(ReaperError::Unkillable { pid })
    }

    fn kill_pid(pid: u32, signal: libc::c_int) -> io::Result<()> {
        if pid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to signal pid 0 or 1",
            ));
        }
        if unsafe { libc::kill(pid as libc::pid_t, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// True when no *live* process remains in the group.
    ///
    /// `killpg(pgid, 0)` is the portable form of the question, but it answers "does the
    /// group exist", and a zombie keeps its group alive. Under a container whose pid 1
    /// does not reap, that difference is the gap between "cleanup finished" and "cleanup
    /// times out every single time", so on Linux the answer comes from `/proc` where
    /// zombies can be told apart.
    pub fn process_group_is_empty(pgid: u32) -> bool {
        if pgid <= 1 {
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return false;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                if let Some(parsed) = parse_proc_stat(&stat) {
                    if parsed.pgrp == pgid && parsed.state != 'Z' {
                        return false;
                    }
                }
            }
            true
        }
        #[cfg(not(target_os = "linux"))]
        {
            let rc = unsafe { libc::killpg(pgid as libc::pid_t, 0) };
            if rc == 0 {
                return false;
            }
            io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    /// Reaps zombies belonging to one process group.
    ///
    /// `waitpid(-pgid, ...)` waits only for children in that group, which is what makes
    /// this safe to call while other `Child` handles are outstanding: it cannot steal the
    /// exit status of a child in a different group. It matters once
    /// [`install_child_subreaper`] is in effect, because orphaned grandchildren then
    /// reparent to the daemon and nothing else would ever reap them.
    fn reap_group_zombies(pgid: u32) -> bool {
        if pgid <= 1 {
            return false;
        }
        let mut reaped = false;
        loop {
            let mut status: libc::c_int = 0;
            let rc = unsafe { libc::waitpid(-(pgid as libc::pid_t), &mut status, libc::WNOHANG) };
            if rc <= 0 {
                return reaped;
            }
            reaped = true;
        }
    }

    /// Blocks until the process identified by `identity` is gone.
    ///
    /// Identity, not pid: a bare pid poll wakes up one day to find the pid reused and
    /// concludes the parent is still alive forever.
    pub fn watch_until_gone(identity: &ProcessIdentity, poll: Duration) {
        while identity.still_running() {
            std::thread::sleep(poll);
        }
    }

    /// Layer 1's fallback for a process that is not our direct child.
    ///
    /// `PR_SET_PDEATHSIG` only fires for the immediate parent, so a helper started by an
    /// agent (rather than by us) has to watch the daemon's pid itself — the `--watch-pid`
    /// convention. The identity is snapshotted now, while the parent is known to be
    /// alive, so a later pid reuse cannot be mistaken for it.
    pub fn spawn_parent_watchdog(
        parent_pid: u32,
        poll: Duration,
        exit_code: i32,
    ) -> Result<std::thread::JoinHandle<()>, ReaperError> {
        let identity = ProcessIdentity::of_pid(parent_pid).ok_or_else(|| {
            ReaperError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("parent pid {parent_pid} is already gone"),
            ))
        })?;
        Ok(std::thread::spawn(move || {
            watch_until_gone(&identity, poll);
            std::process::exit(exit_code);
        }))
    }

    fn exe_name_of(cmd: &Command) -> String {
        Path::new(cmd.get_program())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[cfg(unix)]
pub use unix_impl::{
    install_child_subreaper, process_group_is_empty, spawn_parent_watchdog, watch_until_gone,
    SupervisedChild,
};

// -----------------------------------------------------------------------------------
// Windows
// -----------------------------------------------------------------------------------

/// Windows has no process groups with these semantics and no `PR_SET_PDEATHSIG`. The
/// equivalent design, for when Windows ships:
///
/// * create a Job Object per agent with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the
///   whole tree dies with the daemon's handle — this is the real analogue of both layer 1
///   and layer 2, and it is strictly better than either;
/// * spawn with `CREATE_SUSPENDED`, `AssignProcessToJobObject`, then `ResumeThread`, so
///   there is no window in which the child exists outside the job;
/// * for layer 3, the registry stays as-is but identity has to be checked with the
///   process creation time from `GetProcessTimes` instead of `/proc`.
///
/// It is deliberately unimplemented rather than silently degraded: a Windows build that
/// merely leaks processes would look like it worked.
#[cfg(windows)]
mod windows_impl {
    use super::*;

    impl Supervisor {
        pub fn spawn_supervised(&self, _cmd: Command) -> Result<SupervisedChild, ReaperError> {
            Err(ReaperError::Unimplemented(
                "Windows needs a Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
            ))
        }

        pub fn sweep_orphans(_registry_path: &Path) -> Result<SweepReport, ReaperError> {
            Err(ReaperError::Unimplemented(
                "Windows orphan sweep needs GetProcessTimes-based identity",
            ))
        }
    }

    pub fn install_child_subreaper() -> Result<(), ReaperError> {
        Err(ReaperError::Unimplemented(
            "Windows uses Job Objects instead of subreapers",
        ))
    }

    pub struct SupervisedChild {
        _never: std::convert::Infallible,
    }

    impl SupervisedChild {
        pub fn terminate(&mut self, _grace: Duration) -> Result<Termination, ReaperError> {
            Err(ReaperError::Unimplemented("Windows"))
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{install_child_subreaper, SupervisedChild};

// -----------------------------------------------------------------------------------
// Identity and registry
// -----------------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `/proc/<pid>/comm` is truncated to 15 characters, so a long executable name read back
/// from `comm` will never equal the name we recorded from the full path.
const COMM_MAX: usize = 15;

impl ProcessIdentity {
    /// Snapshots a live process, or `None` if there is nothing there.
    ///
    /// A zombie counts as gone: it holds no resources and cannot be killed, and treating
    /// it as alive makes every cleanup path wait for something that will never happen.
    pub fn of_pid(pid: u32) -> Option<ProcessIdentity> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let parsed = parse_proc_stat(&stat)?;
            if parsed.state == 'Z' {
                return None;
            }
            let exe_name = std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .or_else(|| {
                    std::fs::read_to_string(format!("/proc/{pid}/comm"))
                        .ok()
                        .map(|c| c.trim_end().to_string())
                })?;
            Some(ProcessIdentity {
                pid,
                exe_name,
                started_at_unix_ms: now_ms(),
                start_ticks: Some(parsed.start_ticks),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Without /proc there is no start time to compare, so identity rests on the
            // executable name alone and the sweep is correspondingly weaker. macOS would
            // use proc_pidinfo() here.
            if !pid_is_running(pid) {
                return None;
            }
            Some(ProcessIdentity {
                pid,
                exe_name: String::new(),
                started_at_unix_ms: now_ms(),
                start_ticks: None,
            })
        }
    }

    /// Whether an observed process is the one this identity was taken from.
    ///
    /// Both halves must agree. The start time alone would be enough on Linux, but it is
    /// absent on other platforms and absent from registries written by older versions, so
    /// the name check is what holds when it is missing.
    pub fn matches(&self, observed: &ProcessIdentity) -> bool {
        if self.pid != observed.pid {
            return false;
        }
        if let (Some(recorded), Some(current)) = (self.start_ticks, observed.start_ticks) {
            if recorded != current {
                return false;
            }
        }
        exe_names_match(&self.exe_name, &observed.exe_name)
    }

    pub fn still_running(&self) -> bool {
        match ProcessIdentity::of_pid(self.pid) {
            Some(current) => self.matches(&current),
            None => false,
        }
    }
}

/// Compares executable names, tolerating `/proc/<pid>/comm` truncation at 15 characters.
fn exe_names_match(a: &str, b: &str) -> bool {
    // An empty name means the executable could not be determined, which is a failure to
    // establish identity and must never read as a match. This is also why the sweep does
    // not kill anything on a platform without /proc: not killing is the safe answer.
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    short.len() == COMM_MAX && long.starts_with(short)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcStat {
    state: char,
    pgrp: u32,
    start_ticks: u64,
}

/// Extracts state, process group and start time from `/proc/<pid>/stat`.
///
/// The comm field is wrapped in parentheses and may itself contain spaces *and*
/// parentheses (`(weird (name) here)` is a legal comm), so the fields after it can only
/// be located from the last `)` in the line — splitting on whitespace from the left is
/// the classic way to read the wrong field here.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<ProcStat> {
    let close = stat.rfind(')')?;
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state), so field n is rest[n - 3]: pgrp is field 5 and
    // starttime is field 22.
    Some(ProcStat {
        state: rest.first()?.chars().next()?,
        pgrp: rest.get(2)?.parse().ok()?,
        start_ticks: rest.get(19)?.parse().ok()?,
    })
}

/// Liveness by pid alone. Only ever used where identity has already been established, or
/// on platforms with no `/proc`.
pub fn pid_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            // Distinguishes a zombie from a live process, which `kill(pid, 0)` cannot.
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => match parse_proc_stat(&stat) {
                    Some(parsed) => parsed.state != 'Z',
                    None => false,
                },
                Err(_) => false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if rc == 0 {
                return true;
            }
            io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Installs the child-side death signal. Must be called from inside `pre_exec`.
///
/// Exposed separately from [`Supervisor::spawn_supervised`] because a caller that needs piped
/// stdio for a protocol conversation builds its own command and cannot hand it over — and it
/// still needs this, since without it a child survives a `SIGKILL`ed daemon indefinitely.
///
/// Everything it does must be async-signal-safe: between `fork` and `exec` the child shares the
/// parent's address space and may not allocate or take locks.
#[cfg(unix)]
pub fn arm_child_death_signal(parent_pid: u32) -> io::Result<()> {
    unsafe {
        #[cfg(target_os = "linux")]
        if libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        // The race the signal cannot close by itself: if the parent died between fork and the
        // call above then the signal was already not sent and never will be, so the child has to
        // notice on its own and leave.
        if libc::getppid() != parent_pid as libc::pid_t {
            libc::_exit(EXIT_PARENT_ALREADY_GONE);
        }
    }
    Ok(())
}

/// Records a process this daemon spawned, so the next boot can find it if we die badly.
///
/// Separate from [`Supervisor::spawn_supervised`] because a caller that needs piped stdio for a
/// protocol conversation builds its own command and cannot hand it over. Without this the
/// boot-side sweep reads an empty registry forever, and the third layer of cleanup — the one that
/// covers a hard crash — is present in the code and absent in effect.
pub fn record_spawned(
    registry_path: &Path,
    pid: u32,
    pgid: u32,
    label: &str,
) -> Result<(), ReaperError> {
    let _ = label;
    // A process that is already gone needs no entry, and recording one would leave a stale row
    // that the next boot has to reason about for nothing.
    let Some(identity) = ProcessIdentity::of_pid(pid) else {
        return Ok(());
    };
    let registry = Registry { path: registry_path.to_path_buf(), lock: Mutex::new(()) };
    registry.insert(RegistryEntry { identity, pgid })
}

/// Drops a process from the registry after it has been reaped.
///
/// Best effort by nature: a stale entry costs one identity check on the next boot, which is
/// exactly the check that exists to make stale entries harmless.
pub fn forget_spawned(registry_path: &Path, pid: u32) -> Result<(), ReaperError> {
    let registry = Registry { path: registry_path.to_path_buf(), lock: Mutex::new(()) };
    registry.remove(pid)
}

impl Registry {
    fn insert(&self, entry: RegistryEntry) -> Result<(), ReaperError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = read_registry(&self.path)?;
        file.entries
            .retain(|e| e.identity.pid != entry.identity.pid);
        file.entries.push(entry);
        write_registry(&self.path, &file)
    }

    fn remove(&self, pid: u32) -> Result<(), ReaperError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = read_registry(&self.path)?;
        let before = file.entries.len();
        file.entries.retain(|e| e.identity.pid != pid);
        if file.entries.len() == before {
            return Ok(());
        }
        write_registry(&self.path, &file)
    }
}

fn read_registry(path: &Path) -> Result<RegistryFile, ReaperError> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(RegistryFile::default()),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ReaperError::RegistryFormat {
            path: path.to_path_buf(),
            source,
        }),
        // A first run has no registry, and a first run is not an error.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(RegistryFile::default()),
        Err(source) => Err(ReaperError::Registry {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes through a temporary file and a rename.
///
/// A registry truncated by a crash mid-write is worse than no registry: the pids that
/// were lost are exactly the processes that will never be cleaned up.
fn write_registry(path: &Path, file: &RegistryFile) -> Result<(), ReaperError> {
    let json = serde_json::to_vec_pretty(file).map_err(|source| ReaperError::RegistryFormat {
        path: path.to_path_buf(),
        source,
    })?;
    let tmp = path.with_extension("tmp");
    let err = |source: io::Error| ReaperError::Registry {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
    }
    std::fs::write(&tmp, &json).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_truncation_does_not_break_identity() {
        assert!(exe_names_match("sleep", "sleep"));
        // 20 characters on disk, 15 in /proc/<pid>/comm.
        assert!(exe_names_match("agent-with-long-name", "agent-with-long"));
        assert!(!exe_names_match("sleep", "sleepy"));
        assert!(!exe_names_match("sleep", ""));
        assert!(!exe_names_match("", ""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_is_parsed_from_the_last_paren() {
        // A comm containing spaces and parentheses is legal, and reading fields from the
        // left of the line finds the wrong one.
        let line = "1234 (weird (name) here) S 1 4321 1234 0 -1 4194560 100 0 0 0 1 2 3 4 \
                    20 0 1 0 987654 1000 100 200 0 0 0 0 0 0";
        let parsed = parse_proc_stat(line).unwrap();
        assert_eq!(parsed.state, 'S');
        assert_eq!(parsed.pgrp, 4321);
        assert_eq!(parsed.start_ticks, 987654);

        // Cross-check the field offsets against this process rather than trusting the
        // hand-written line above.
        let me = std::process::id();
        let live =
            parse_proc_stat(&std::fs::read_to_string(format!("/proc/{me}/stat")).unwrap()).unwrap();
        let expected_pgrp = unsafe { libc::getpgid(me as libc::pid_t) } as u32;
        assert_eq!(live.pgrp, expected_pgrp);
        assert!(live.start_ticks > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_of_this_process_is_stable_and_self_matching() {
        let me = ProcessIdentity::of_pid(std::process::id()).unwrap();
        assert!(me.start_ticks.is_some());
        assert!(me.still_running());

        let impostor = ProcessIdentity {
            start_ticks: me.start_ticks.map(|t| t + 1),
            ..me.clone()
        };
        assert!(
            !impostor.still_running(),
            "a different start time must not match, or pid reuse goes undetected"
        );

        let renamed = ProcessIdentity {
            exe_name: "something-else".to_string(),
            ..me.clone()
        };
        assert!(!renamed.still_running());
    }

    #[test]
    fn a_missing_registry_reads_as_empty() {
        let file = read_registry(Path::new("/nonexistent/wkbd/registry.json")).unwrap();
        assert!(file.entries.is_empty());
    }

    #[test]
    fn registry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/registry.json");
        let entry = RegistryEntry {
            identity: ProcessIdentity {
                pid: 4242,
                exe_name: "fake-acp-agent".to_string(),
                started_at_unix_ms: 1_700_000_000_000,
                start_ticks: Some(99),
            },
            pgid: 4242,
        };
        write_registry(
            &path,
            &RegistryFile {
                version: 1,
                entries: vec![entry.clone()],
            },
        )
        .unwrap();

        let back = read_registry(&path).unwrap();
        assert_eq!(back.entries, vec![entry]);
        // Flattened, so the on-disk shape is the documented {pid, exe_name, started_at}
        // record rather than a nested object.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"pid\": 4242"), "{text}");
        assert!(text.contains("\"exe_name\": \"fake-acp-agent\""), "{text}");
        assert!(text.contains("\"started_at_unix_ms\""), "{text}");
    }
}
