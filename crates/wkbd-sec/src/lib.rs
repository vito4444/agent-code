//! Security boundary layer for the workbench daemon.
//!
//! Every module here exists because a shipped product got the same thing wrong. The
//! daemon runs AI coding agents as child processes and executes privileged operations
//! (real disk I/O, real `git` invocations, real process lifetimes) on their behalf, so
//! the agent's request is untrusted input in the same sense that a network request is.
//!
//! The modules are deliberately independent: none of them calls into another, so a
//! mistake in one cannot silently weaken another.
//!
//! * [`path_guard`] — decides whether a filesystem path an agent asked us to touch is
//!   inside the boundary, and hands back an already-open file descriptor so the answer
//!   cannot go stale between the check and the use.
//! * [`git_env`] — builds `git` invocations whose environment and configuration are
//!   constructed from nothing rather than inherited and trimmed.
//! * [`reaper`] — three independent layers of subprocess cleanup, because each one has a
//!   failure mode the others cover.
//! * [`permission`] — binds a remembered approval to the exact bytes that were approved.
//! * [`text_sanitize`] — strips invisible characters out of anything shown to a human
//!   before they approve it, and reports what it stripped.

pub mod git_env;
pub mod path_guard;
pub mod permission;
pub mod reaper;
pub mod text_sanitize;
