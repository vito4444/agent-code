//! The single place this crate spawns `git`.
//!
//! Everything goes through here so that three hardening decisions cannot be forgotten at
//! a call site:
//!
//! - **`-c core.hooksPath=/dev/null` on every invocation.** Measured on this machine:
//!   `git worktree add` executes `.git/hooks/post-checkout` (docs/M0-FINDINGS.md §2.1),
//!   and `.git` is shared by every worktree, so a hook dropped by one agent runs inside
//!   the daemon on behalf of every other agent. Creating worktrees is our most frequent
//!   privileged operation, which makes it the worst possible thing to leave hookable.
//! - **`-c core.quotePath=false`.** Measured: without it, `merge-tree` and friends return
//!   non-ASCII filenames C-quoted (`"unic\303\266de.txt"`), so a conflict on a file with
//!   a non-ASCII name would be reported under a path that does not exist on disk. Note
//!   this is not sufficient on its own — git still quotes names containing `"`, `\` or
//!   control characters, which is why the parsers call [`unquote_c_style`].
//! - **Arguments as an array, never a shell string.** Branch names and paths come from
//!   models and from the filesystem.
//!
//! Environment hygiene matters for the same reason: the daemon can be started from
//! inside a git context (a hook, a rebase, an IDE task), and an inherited `GIT_DIR` or
//! `GIT_INDEX_FILE` would silently redirect every operation here at some other
//! repository — including the ones that write objects. What this does is remove the
//! variables that can redirect git; it deliberately does not build an environment from
//! nothing, because these invocations are the daemon's own and the user's global git
//! configuration (credential helpers, `include.path`) has to keep working. Git commands
//! run *on behalf of an agent* are a different threat model and belong behind
//! `wkbd-sec::git_env`, not here.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::error::{Result, VcsError};

/// Prepended to every argument list. `-c` settings win over both the repository config
/// and the user's global config, which is what we want: an agent that writes
/// `core.hooksPath` into the shared `.git/config` must not be able to re-enable hooks.
const HARDENING: [&str; 4] = ["-c", "core.hooksPath=/dev/null", "-c", "core.quotePath=false"];

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub code: Option<i32>,
    /// Raw bytes: several formats we parse are NUL-separated, and paths on Linux are
    /// bytes. Converting eagerly with `to_string_lossy` would turn a path we cannot
    /// represent into a path that does not exist, which is worse than an error.
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub invocation: String,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_utf8(&self) -> Result<&str> {
        std::str::from_utf8(&self.stdout).map_err(|e| VcsError::Parse {
            invocation: self.invocation.clone(),
            detail: format!("output is not valid UTF-8: {e}"),
        })
    }

    pub fn stdout_trimmed(&self) -> Result<&str> {
        Ok(self.stdout_utf8()?.trim_end_matches(['\n', '\r']))
    }

    /// NUL-separated fields, empty trailing field dropped. Used for every `-z` format.
    pub fn nul_fields(&self) -> Result<Vec<String>> {
        self.stdout_utf8()?
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| Ok(s.to_string()))
            .collect()
    }

    pub fn error(&self) -> VcsError {
        VcsError::Git {
            invocation: self.invocation.clone(),
            status: match self.code {
                Some(c) => format!("exit code {c}"),
                None => "killed by signal".to_string(),
            },
            stderr: first_lines(&self.stderr, 5),
        }
    }
}

/// Runs git in `cwd`. Non-zero exit codes are returned, not raised: `merge-tree` uses
/// exit code 1 to mean "conflict", which is an expected answer rather than a failure.
pub(crate) fn run<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<GitOutput> {
    run_with_env(cwd, args, &[])
}

pub(crate) fn run_with_env<S: AsRef<OsStr>>(
    cwd: &Path,
    args: &[S],
    env: &[(&str, &str)],
) -> Result<GitOutput> {
    let invocation = describe(args);
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.args(HARDENING.iter().map(OsStr::new));
    for a in args {
        cmd.arg(a.as_ref());
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    for leaked in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
    ] {
        cmd.env_remove(leaked);
    }
    // A credential prompt on a daemon with no terminal blocks forever and takes the
    // agent slot with it; failing fast is always the better outcome here.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // We match on git's own English wording in a few places (branch already checked out,
    // worktree not clean). A translated git would silently turn those into generic
    // failures, so the message locale is pinned rather than inherited.
    cmd.env("LC_ALL", "C");

    tracing::debug!(cwd = %cwd.display(), invocation = %invocation, "git");

    let out = cmd.output().map_err(|source| VcsError::Spawn {
        invocation: invocation.clone(),
        source,
    })?;

    Ok(GitOutput {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        invocation,
    })
}

/// As [`run`], but any non-zero exit is an error. Use for commands where "it did not
/// work" has no second meaning.
pub(crate) fn run_checked<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<GitOutput> {
    let out = run(cwd, args)?;
    if out.success() {
        Ok(out)
    } else {
        Err(out.error())
    }
}

/// Resolves any commit-ish to a full object id. Callers pin their inputs with this
/// before doing anything in two steps: a branch name can move between a prediction and
/// the commit built from it, and an integration commit that silently used a different
/// tip than the one predicted is the exact failure this crate exists to prevent.
pub(crate) fn resolve_commit(repo: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    let out = run(repo, &["rev-parse", "--verify", "--quiet", &spec])?;
    let oid = out.stdout_trimmed()?.to_string();
    if !out.success() || oid.is_empty() {
        return Err(VcsError::Invalid(format!("`{rev}` does not name a commit in this repository")));
    }
    Ok(oid)
}

pub(crate) fn toplevel(repo_or_worktree: &Path) -> Result<std::path::PathBuf> {
    let out = run_checked(repo_or_worktree, &["rev-parse", "--show-toplevel"])?;
    let top = out.stdout_trimmed()?;
    if top.is_empty() {
        return Err(VcsError::Invalid(format!(
            "{} is not inside a git working tree",
            repo_or_worktree.display()
        )));
    }
    Ok(std::path::PathBuf::from(top))
}

/// git object ids are lowercase hex, 40 chars for sha-1 and 64 for the sha-256 object
/// format. Anything else is not an oid, and treating it as one is how a parser turns an
/// error message into a "successful" result.
pub(crate) fn looks_like_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Reverses git's C-style path quoting. Needed even with `core.quotePath=false`, which
/// only suppresses quoting of non-ASCII bytes: measured, a file named `we"ird\back.txt`
/// still comes back as `"we\"ird\\back.txt"`.
pub(crate) fn unquote_c_style(field: &str) -> Result<String> {
    if !field.starts_with('"') {
        return Ok(field.to_string());
    }
    let body = field
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| VcsError::Parse {
            invocation: "quoted path".to_string(),
            detail: format!("unterminated quoted path: {field}"),
        })?;

    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('a') => out.push(0x07),
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('v') => out.push(0x0b),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some(d) if d.is_digit(8) => {
                // Octal escapes are emitted per byte, so a multi-byte character arrives
                // as several of them and must be reassembled before UTF-8 decoding.
                let mut value = d.to_digit(8).unwrap();
                for _ in 0..2 {
                    match chars.clone().next() {
                        Some(n) if n.is_digit(8) => {
                            value = value * 8 + n.to_digit(8).unwrap();
                            chars.next();
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            other => {
                return Err(VcsError::Parse {
                    invocation: "quoted path".to_string(),
                    detail: format!("unknown escape `\\{}` in {field}", other.unwrap_or('?')),
                })
            }
        }
    }
    String::from_utf8(out).map_err(|e| VcsError::Parse {
        invocation: "quoted path".to_string(),
        detail: format!("quoted path is not UTF-8: {e}"),
    })
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| OsString::from(a.as_ref()).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn first_lines(text: &str, n: usize) -> String {
    text.lines().filter(|l| !l.trim().is_empty()).take(n).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_octal_multibyte() {
        assert_eq!(unquote_c_style(r#""unic\303\266de-\345\220\215.txt""#).unwrap(), "unicöde-名.txt");
    }

    #[test]
    fn unquotes_quote_and_backslash() {
        assert_eq!(unquote_c_style(r#""we\"ird\\back.txt""#).unwrap(), r#"we"ird\back.txt"#);
    }

    #[test]
    fn leaves_unquoted_names_alone() {
        assert_eq!(unquote_c_style("dir with space/f ile.txt").unwrap(), "dir with space/f ile.txt");
    }

    #[test]
    fn oid_shape() {
        assert!(looks_like_oid("e428495f17383d4a9e5d1c56de14b151740220be"));
        assert!(looks_like_oid(&"a".repeat(64)));
        assert!(!looks_like_oid("merge-tree: nosuchref - not something we can merge"));
        assert!(!looks_like_oid(""));
    }
}
