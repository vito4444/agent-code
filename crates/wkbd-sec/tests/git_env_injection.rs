//! `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` inject configuration
//! at git's command scope, which is *protected* configuration. This file proves against a
//! real `git` process that the allowlisted environment leaves none of it behind.
//!
//! It lives in its own test binary because it mutates the process environment, and
//! mutating the environment while another thread is spawning a process is not something
//! to do inside a shared binary.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use wkbd_sec::git_env::GitCommand;

fn raw_git(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=wkbd-test",
            "-c",
            "user.email=wkbd@example.invalid",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be installed to run these tests")
}

#[test]
fn command_scope_environment_injection_does_not_reach_git() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    let home = base.join("sandbox-home");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(&home).unwrap();
    assert!(raw_git(&repo, &["init", "-q"]).status.success());

    std::env::set_var("GIT_CONFIG_COUNT", "2");
    std::env::set_var("GIT_CONFIG_KEY_0", "core.pager");
    std::env::set_var("GIT_CONFIG_VALUE_0", "sh -c 'touch /tmp/wkbd-pwned'");
    std::env::set_var("GIT_CONFIG_KEY_1", "safe.directory");
    std::env::set_var("GIT_CONFIG_VALUE_1", "*");

    // Positive control: this is what the injection does to an ordinary git process.
    let control = raw_git(&repo, &["config", "--get", "core.pager"]);
    assert_eq!(
        String::from_utf8_lossy(&control.stdout).trim(),
        "sh -c 'touch /tmp/wkbd-pwned'",
        "the injection did not take effect, so the negative case proves nothing"
    );
    // safe.directory only has any effect at all when it arrives through protected
    // configuration, which is what makes this environment channel worth closing.
    let control = raw_git(&repo, &["config", "--get", "safe.directory"]);
    assert_eq!(String::from_utf8_lossy(&control.stdout).trim(), "*");

    let guarded = GitCommand::new("config")
        .unwrap()
        .home(&home)
        .current_dir(&repo)
        .args(["--get", "core.pager"])
        .build()
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&guarded.stdout).trim(),
        "cat",
        "injected core.pager survived into the guarded invocation"
    );

    let guarded = GitCommand::new("config")
        .unwrap()
        .home(&home)
        .current_dir(&repo)
        .args(["--get", "safe.directory"])
        .build()
        .unwrap()
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&guarded.stdout).trim().is_empty(),
        "injected safe.directory survived into the guarded invocation"
    );

    // Nothing at all should remain of the GIT_* namespace.
    let guarded = GitCommand::new("config")
        .unwrap()
        .home(&home)
        .current_dir(&repo)
        .args(["--list", "--show-scope"])
        .build()
        .unwrap()
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&guarded.stdout);
    assert!(
        !listing.contains("wkbd-pwned"),
        "injected value present in effective config: {listing}"
    );

    for key in [
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_KEY_1",
        "GIT_CONFIG_VALUE_1",
    ] {
        std::env::remove_var(key);
    }
}
