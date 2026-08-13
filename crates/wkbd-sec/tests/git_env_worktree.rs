//! End-to-end proof against a real repository that `git worktree add` issued through
//! [`GitCommand`] does not run `.git/hooks/post-checkout`.
//!
//! Every case carries a positive control that runs the same operation through plain
//! `git`, because a test that only asserts "the hook did not fire" also passes when the
//! hook was never wired up correctly in the first place.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wkbd_sec::git_env::GitCommand;

fn raw_git(cwd: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=wkbd-test",
            "-c",
            "user.email=wkbd@example.invalid",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

struct Fixture {
    _tmp: TempDir,
    base: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    hook_log: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    let home = base.join("sandbox-home");
    let hook_log = base.join("hook_fired.log");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(&home).unwrap();

    raw_git(&repo, &["init", "-q"]);
    fs::write(repo.join("a.txt"), b"contents\n").unwrap();
    raw_git(&repo, &["add", "a.txt"]);
    raw_git(&repo, &["commit", "-q", "-m", "init"]);

    // Measured in docs/M0-FINDINGS.md 2.1: this is the hook `git worktree add` runs, out
    // of the hooks directory that every worktree shares.
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf 'post-checkout fired cwd=%s\\n' \"$PWD\" >> {}\n",
            hook_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    Fixture {
        _tmp: tmp,
        base,
        repo,
        home,
        hook_log,
    }
}

#[test]
fn post_checkout_fires_for_plain_git_and_not_for_a_guarded_invocation() {
    let fx = fixture();

    // Positive control. If this stops firing, the negative assertion below has stopped
    // meaning anything.
    let control_wt = fx.base.join("wt-control");
    raw_git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "control",
            control_wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(
        fx.hook_log.exists(),
        "plain `git worktree add` did not run post-checkout; the fixture is broken, not the guard"
    );
    let control_log = fs::read_to_string(&fx.hook_log).unwrap();
    assert!(control_log.contains("post-checkout fired"), "{control_log}");
    fs::remove_file(&fx.hook_log).unwrap();

    // The same operation through the guarded builder.
    let guarded_wt = fx.base.join("wt-guarded");
    let out = GitCommand::new("worktree")
        .unwrap()
        .home(&fx.home)
        .current_dir(&fx.repo)
        .args(["add", "-b", "guarded", guarded_wt.to_str().unwrap(), "HEAD"])
        .build()
        .unwrap()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "guarded worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The work really happened...
    assert!(
        guarded_wt.join("a.txt").exists(),
        "the worktree was not created, so proving the hook did not run proves nothing"
    );
    // ...and the hook did not.
    assert!(
        !fx.hook_log.exists(),
        "post-checkout ran under the guarded invocation: {}",
        fs::read_to_string(&fx.hook_log).unwrap_or_default()
    );
}

#[test]
fn config_in_the_shared_git_config_is_overridden_at_command_scope() {
    let fx = fixture();

    // Exactly the measured attack from docs/M0-FINDINGS.md 2.2: any agent with write
    // access to any worktree can set this once, for everybody.
    raw_git(
        &fx.repo,
        &["config", "core.pager", "sh -c 'touch /tmp/wkbd-pwned'"],
    );

    let control = raw_git(&fx.repo, &["config", "--get", "core.pager"]);
    assert_eq!(
        String::from_utf8_lossy(&control.stdout).trim(),
        "sh -c 'touch /tmp/wkbd-pwned'",
        "the fixture failed to plant the config"
    );

    let out = GitCommand::new("config")
        .unwrap()
        .home(&fx.home)
        .current_dir(&fx.repo)
        .args(["--get", "core.pager"])
        .build()
        .unwrap()
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "cat",
        "command-scope -c did not win over the repository's .git/config"
    );
}

#[test]
fn the_sandbox_home_is_what_git_sees() {
    let fx = fixture();

    // A global config that would be read if HOME leaked through.
    let real_home_config = fx.base.join("decoy-home");
    fs::create_dir_all(real_home_config.join("xdg/git")).unwrap();
    fs::write(
        real_home_config.join(".gitconfig"),
        "[user]\n\tname = leaked-from-home\n",
    )
    .unwrap();
    fs::write(
        fx.home.join(".gitconfig"),
        "[user]\n\tname = sandbox-home\n",
    )
    .unwrap();

    let out = GitCommand::new("config")
        .unwrap()
        .home(&fx.home)
        .current_dir(&fx.repo)
        .args(["--get", "user.name"])
        .build()
        .unwrap()
        .output()
        .unwrap();

    // GIT_CONFIG_GLOBAL=/dev/null means even the sandbox HOME's own .gitconfig is not
    // read; the point of the assertion is that *no* file outside the repository is.
    assert!(
        !out.status.success(),
        "user.name resolved to {:?}; some global config file was read",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}
