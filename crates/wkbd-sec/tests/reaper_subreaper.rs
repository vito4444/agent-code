//! `PR_SET_CHILD_SUBREAPER` is process-wide and permanent for the process that sets it,
//! so this lives in its own test binary: turning it on inside the shared one would change
//! the reparenting behaviour every other test observes.

#![cfg(target_os = "linux")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use wkbd_sec::reaper::{
    install_child_subreaper, pid_is_running, process_group_is_empty, Supervisor,
};

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

/// Parent pid straight out of `/proc`, so the assertion does not depend on the crate's
/// own parsing being right.
fn ppid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(1)?.parse().ok()
}

#[test]
fn orphaned_grandchildren_reparent_to_the_daemon_and_are_still_reachable() {
    install_child_subreaper().expect("PR_SET_CHILD_SUBREAPER");

    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let supervisor = Supervisor::with_registry(dir.path().join("registry.json"));

    // The shell exits immediately and leaves the sleep behind. Without a subreaper the
    // orphan reparents to pid 1 and the daemon can no longer see it as its own.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("sleep 300 & echo $! > {}", pid_file.display()));
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = supervisor.spawn_supervised(cmd).unwrap();
    let pgid = child.pgid();

    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&pid_file)
            .map(|s| s.trim().parse::<u32>().is_ok())
            .unwrap_or(false)
    }));
    let grandchild: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let us = std::process::id();
    assert!(
        wait_until(Duration::from_secs(5), || ppid_of(grandchild) == Some(us)),
        "grandchild {grandchild} reparented to {:?}, not to this process ({us})",
        ppid_of(grandchild)
    );

    child.terminate(Duration::from_secs(2)).unwrap();

    assert!(!pid_is_running(grandchild));
    // Reparented orphans become *our* zombies once the subreaper is installed, so
    // termination has to reap them; otherwise the process table fills up with exactly the
    // processes this layer was added to clean up.
    assert!(
        wait_until(Duration::from_secs(2), || ppid_of(grandchild).is_none()),
        "grandchild {grandchild} is still a zombie after terminate"
    );
    assert!(process_group_is_empty(pgid));
}
