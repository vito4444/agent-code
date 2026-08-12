//! Real processes, real signals, real `/proc`.
//!
//! The failures these guard against are the ones that only appear with a real process
//! tree: a shell that forked a background job and passes nothing on when it is signalled,
//! and a boot-time sweep that kills a pid which now belongs to somebody else.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use wkbd_sec::reaper::{pid_is_running, process_group_is_empty, Supervisor, Termination};

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

fn read_pid_file(path: &Path) -> u32 {
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(path)
                .map(|s| s.trim().parse::<u32>().is_ok())
                .unwrap_or(false)
        }),
        "the fixture never wrote its background pid to {}",
        path.display()
    );
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

#[test]
fn terminate_takes_the_whole_process_group_and_not_just_the_child() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let supervisor = Supervisor::with_registry(dir.path().join("registry.json"));

    // A shell with a background job: signalling only the shell leaves `sleep 300`
    // running, which is precisely the leak that ran a shipped product out of ptys.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(
        "sleep 300 & echo $! > {}; sleep 300",
        pid_file.display()
    ));
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = supervisor.spawn_supervised(cmd).unwrap();
    let shell_pid = child.pid();
    let pgid = child.pgid();
    let grandchild = read_pid_file(&pid_file);

    assert_ne!(grandchild, shell_pid);
    assert!(pid_is_running(grandchild));
    assert!(!process_group_is_empty(pgid));

    let outcome = child.terminate(Duration::from_secs(2)).unwrap();
    assert_eq!(outcome, Termination::ExitedOnSigterm);

    assert!(
        !pid_is_running(grandchild),
        "the backgrounded grandchild {grandchild} survived termination"
    );
    assert!(
        process_group_is_empty(pgid),
        "process group {pgid} still has live members"
    );
}

#[test]
fn the_registry_gains_an_entry_on_spawn_and_loses_it_on_terminate() {
    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("registry.json");
    let supervisor = Supervisor::with_registry(&registry);

    let mut cmd = Command::new("sleep");
    cmd.arg("300").stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = supervisor.spawn_supervised(cmd).unwrap();

    let written = std::fs::read_to_string(&registry).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
    let entry = &parsed["entries"][0];
    assert_eq!(entry["pid"].as_u64().unwrap() as u32, child.pid());
    assert_eq!(entry["exe_name"].as_str().unwrap(), "sleep");
    assert_eq!(entry["pgid"].as_u64().unwrap() as u32, child.pgid());
    assert!(entry["started_at_unix_ms"].as_u64().unwrap() > 0);
    assert!(entry["start_ticks"].as_u64().is_some());

    child.terminate(Duration::from_secs(2)).unwrap();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
    assert_eq!(after["entries"].as_array().unwrap().len(), 0);
}

#[test]
fn sweep_kills_an_orphan_whose_identity_still_matches() {
    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("registry.json");
    let supervisor = Supervisor::with_registry(&registry);

    let mut cmd = Command::new("sleep");
    cmd.arg("300").stdout(Stdio::null()).stderr(Stdio::null());
    // Stands in for a process left behind by a previous run: it is in the registry and it
    // is still alive.
    let mut child = supervisor.spawn_supervised(cmd).unwrap();
    let pid = child.pid();

    let report = Supervisor::sweep_orphans(&registry).unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(report.killed, vec![pid], "{report:?}");
    assert!(report.identity_mismatch.is_empty(), "{report:?}");
    assert!(!pid_is_running(pid));

    // The swept process was killed rather than merely forgotten.
    let status = child.try_wait().unwrap().expect("child should have exited");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc_sigterm()));
}

fn libc_sigterm() -> i32 {
    15
}

#[test]
fn sweep_refuses_to_kill_a_pid_that_now_belongs_to_someone_else() {
    // The whole reason the registry stores an executable name: pids get reused, and a
    // sweep that trusts the number alone eventually kills an unrelated process.
    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("registry.json");

    let mut victim = Command::new("sleep")
        .arg("300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = victim.id();

    // Same pid, same start time, different program: exactly what a recycled pid looks
    // like.
    let entry = serde_json::json!({
        "version": 1,
        "entries": [{
            "pid": pid,
            "exe_name": "wkbd-agent-node",
            "started_at_unix_ms": 1_700_000_000_000u64,
            "start_ticks": 12345,
            "pgid": pid,
        }]
    });
    std::fs::write(&registry, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

    let report = Supervisor::sweep_orphans(&registry).unwrap();
    assert_eq!(report.identity_mismatch, vec![pid], "{report:?}");
    assert!(report.killed.is_empty(), "{report:?}");
    assert!(
        pid_is_running(pid),
        "an unrelated process was killed by the sweep"
    );
    assert!(victim.try_wait().unwrap().is_none());

    victim.kill().unwrap();
    victim.wait().unwrap();
}

#[test]
fn sweep_refuses_when_only_the_start_time_disagrees() {
    // The name can legitimately be reused (every agent is the same binary), so the start
    // time is the part that actually distinguishes one run's pid from the next one's.
    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("registry.json");

    let mut victim = Command::new("sleep")
        .arg("300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = victim.id();

    let entry = serde_json::json!({
        "version": 1,
        "entries": [{
            "pid": pid,
            "exe_name": "sleep",
            "started_at_unix_ms": 1_700_000_000_000u64,
            "start_ticks": 1,
            "pgid": pid,
        }]
    });
    std::fs::write(&registry, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

    let report = Supervisor::sweep_orphans(&registry).unwrap();
    assert_eq!(report.identity_mismatch, vec![pid], "{report:?}");
    assert!(pid_is_running(pid));

    victim.kill().unwrap();
    victim.wait().unwrap();
}

#[test]
fn sweep_tolerates_a_pid_that_is_already_gone() {
    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("registry.json");

    let mut short_lived = Command::new("true").spawn().unwrap();
    let pid = short_lived.id();
    short_lived.wait().unwrap();

    let entry = serde_json::json!({
        "version": 1,
        "entries": [{
            "pid": pid,
            "exe_name": "true",
            "started_at_unix_ms": 1_700_000_000_000u64,
            "start_ticks": 99999999,
            "pgid": pid,
        }]
    });
    std::fs::write(&registry, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

    let report = Supervisor::sweep_orphans(&registry).unwrap();
    assert_eq!(report.already_gone, vec![pid], "{report:?}");
    assert!(report.killed.is_empty());
    assert!(report.failed.is_empty());

    // A sweep leaves nothing behind for the next boot to re-examine.
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
    assert_eq!(after["entries"].as_array().unwrap().len(), 0);
}

#[test]
fn sweep_of_a_missing_registry_is_not_an_error() {
    let dir = TempDir::new().unwrap();
    let report = Supervisor::sweep_orphans(&dir.path().join("nothing-here.json")).unwrap();
    assert_eq!(report.examined, 0);
}

#[test]
fn watching_a_pid_returns_once_it_exits() {
    // The `--watch-pid` fallback for processes PR_SET_PDEATHSIG cannot reach.
    let mut child = Command::new("sleep").arg("0.3").spawn().unwrap();
    let identity = wkbd_sec::reaper::ProcessIdentity::of_pid(child.id()).unwrap();

    let started = Instant::now();
    wkbd_sec::reaper::watch_until_gone(&identity, Duration::from_millis(20));
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(150),
        "returned after {elapsed:?}, before the process could have exited"
    );
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    child.wait().unwrap();
}
