//! Glob handling shared by ownership checks, hydration and the immutable-file harness.
//!
//! Two rules are applied everywhere, and they are here rather than at each call site
//! because inconsistency between them would be invisible: a pattern that means one thing
//! when deciding what an agent was allowed to touch and another when deciding what the
//! acceptance harness restores is a hole, not a quirk.
//!
//! 1. `literal_separator(true)`: `*` stops at `/`. Without it `src/*` matches
//!    `src/a/b/c`, so a declaration meant as "one directory" silently authorises the
//!    whole subtree.
//! 2. A pattern with no glob metacharacters is treated as "this path, or anything under
//!    it". Models and humans both write `crates/wkbd-vcs` meaning the directory, and
//!    reading that as a single file produces violation reports nobody believes — and a
//!    check nobody believes gets switched off.

use globset::{GlobBuilder, GlobMatcher};

use crate::error::{Result, VcsError};

#[derive(Debug)]
pub(crate) struct Pattern {
    pub source: String,
    matchers: Vec<GlobMatcher>,
}

impl Pattern {
    pub fn parse(source: &str) -> Result<Pattern> {
        let normalized = normalize(source)?;
        let mut matchers = Vec::with_capacity(2);
        for form in normalized {
            matchers.push(compile(source, &form)?);
        }
        Ok(Pattern { source: source.to_string(), matchers })
    }

    pub fn is_match(&self, relative_path: &str) -> bool {
        self.matchers.iter().any(|m| m.is_match(relative_path))
    }
}

pub(crate) fn parse_all(sources: &[String]) -> Result<Vec<Pattern>> {
    sources.iter().map(|s| Pattern::parse(s)).collect()
}

pub(crate) fn any_match(patterns: &[Pattern], relative_path: &str) -> bool {
    patterns.iter().any(|p| p.is_match(relative_path))
}

fn compile(source: &str, form: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(form)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| VcsError::Glob { pattern: source.to_string(), detail: e.to_string() })?
        .compile_matcher())
}

/// Expands one written pattern into the forms it should match against. Also the only
/// place patterns are rejected: an absolute pattern or one containing `..` can only be
/// an attempt (deliberate or accidental) to name something outside the tree being
/// scanned, and every consumer of this module scans a tree it must not leave.
fn normalize(source: &str) -> Result<Vec<String>> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VcsError::Glob {
            pattern: source.to_string(),
            detail: "empty pattern".to_string(),
        });
    }
    if trimmed.starts_with('/') || trimmed.starts_with('~') {
        return Err(VcsError::Glob {
            pattern: source.to_string(),
            detail: "must be relative to the tree root".to_string(),
        });
    }
    let cleaned = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if cleaned.split('/').any(|c| c == "..") {
        return Err(VcsError::Glob {
            pattern: source.to_string(),
            detail: "`..` is not allowed".to_string(),
        });
    }

    if let Some(dir) = cleaned.strip_suffix('/') {
        return Ok(vec![format!("{dir}/**"), dir.to_string()]);
    }
    if has_meta(cleaned) {
        return Ok(vec![cleaned.to_string()]);
    }
    Ok(vec![cleaned.to_string(), format!("{cleaned}/**")])
}

fn has_meta(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// Rejects `Glob` patterns and keeps concrete relative file paths. Used where a caller
/// must name files rather than describe them — restoring from a commit, for instance,
/// where a pattern would have to be expanded against the commit rather than the disk and
/// the difference decides whether an added file is deleted or left in place.
pub(crate) fn require_concrete(path: &str) -> Result<String> {
    if has_meta(path) {
        return Err(VcsError::UnsafePath {
            path: path.to_string(),
            reason: "a concrete file path is required here, not a glob",
        });
    }
    let normalized = normalize(path)?;
    Ok(normalized[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_does_not_cross_directories() {
        let p = Pattern::parse("src/*.rs").unwrap();
        assert!(p.is_match("src/lib.rs"));
        assert!(!p.is_match("src/deep/lib.rs"));
    }

    #[test]
    fn double_star_crosses_directories() {
        let p = Pattern::parse("src/**/*.rs").unwrap();
        assert!(p.is_match("src/deep/lib.rs"));
        assert!(p.is_match("src/lib.rs"));
    }

    #[test]
    fn bare_path_covers_its_subtree() {
        let p = Pattern::parse("crates/wkbd-vcs").unwrap();
        assert!(p.is_match("crates/wkbd-vcs"));
        assert!(p.is_match("crates/wkbd-vcs/src/lib.rs"));
        assert!(!p.is_match("crates/wkbd-sec/src/lib.rs"));
    }

    #[test]
    fn trailing_slash_is_a_directory() {
        let p = Pattern::parse("tests/").unwrap();
        assert!(p.is_match("tests/test_a.py"));
        assert!(!p.is_match("testsuite/test_a.py"));
    }

    #[test]
    fn escapes_are_refused() {
        assert!(Pattern::parse("../etc/passwd").is_err());
        assert!(Pattern::parse("/etc/passwd").is_err());
        assert!(Pattern::parse("~/.ssh/id_rsa").is_err());
        assert!(Pattern::parse("  ").is_err());
    }
}
