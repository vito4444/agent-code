//! Reading a change so somebody can look at it.
//!
//! Two commits in, per-file before-and-after text out. The rendering is the interface's problem; this
//! only has to produce a faithful pair of versions and be honest about the files it cannot.
//!
//! ## Why text rather than a patch
//!
//! The interface already renders a diff from `(old, new)` and does its own line matching, so handing
//! it a unified patch would mean parsing back out what git just formatted — two parsers for one fact,
//! and the one that goes wrong is the one nobody tests against unusual filenames. Reading the two
//! blobs skips the format entirely.
//!
//! ## What it refuses to do
//!
//! It will not read a whole repository into memory to answer a question about a change. Both the
//! number of files and the size of each are capped, and a file over the cap comes back marked rather
//! than truncated: a truncated diff looks like a small change, which is the one wrong impression a
//! review surface must not give.

use std::path::Path;

use crate::error::Result;
use crate::git;

/// One file, before and after.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    pub path: String,
    /// `None` when the file did not exist in the earlier commit.
    pub old_text: Option<String>,
    /// `None` when the file is gone in the later commit.
    pub new_text: Option<String>,
    /// Why the contents are absent, when they are and it is not because the file was added or
    /// deleted. `binary` or `too-large`.
    pub omitted: Option<String>,
}

/// Everything that changed between two commits.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    pub from: String,
    pub to: String,
    pub files: Vec<FileChange>,
    /// Files that changed and are not in `files` because the cap was reached.
    ///
    /// Reported rather than dropped. A review surface that silently shows the first fifty files of a
    /// sixty-file change is showing a different change from the one being merged.
    pub truncated: usize,
}

/// Largest file this will read either side of.
///
/// A cap rather than a judgement about what is reasonable to review: without one, a change touching
/// a generated lockfile allocates its whole contents twice in the daemon.
const MAX_FILE_BYTES: usize = 512 * 1024;

pub fn changes_between(
    repo: &Path,
    from: &str,
    to: &str,
    max_files: usize,
) -> Result<ChangeSet> {
    let paths = crate::ownership::changed_paths(repo, from, to)?;
    let truncated = paths.len().saturating_sub(max_files);

    let mut files = Vec::new();
    for path in paths.iter().take(max_files) {
        let old = read_blob(repo, from, path)?;
        let new = read_blob(repo, to, path)?;

        // Both sides absent means the path changed in a way neither revision can show as text — a
        // mode change on a file whose contents are identical, most often. Reporting it with no
        // contents is more useful than leaving it out, because it did change.
        let omitted = match (&old, &new) {
            (Blob::TooLarge, _) | (_, Blob::TooLarge) => Some("too-large".to_string()),
            (Blob::Binary, _) | (_, Blob::Binary) => Some("binary".to_string()),
            _ => None,
        };

        files.push(FileChange {
            path: path.clone(),
            old_text: old.text(),
            new_text: new.text(),
            omitted,
        });
    }

    Ok(ChangeSet {
        from: from.to_string(),
        to: to.to_string(),
        files,
        truncated,
    })
}

enum Blob {
    Text(String),
    Missing,
    Binary,
    TooLarge,
}

impl Blob {
    fn text(self) -> Option<String> {
        match self {
            Blob::Text(t) => Some(t),
            _ => None,
        }
    }
}

fn read_blob(repo: &Path, commit: &str, path: &str) -> Result<Blob> {
    let spec = format!("{commit}:{path}");

    // Size first, so an enormous blob is never read at all. `cat-file -s` costs one object header.
    let size = git::run(repo, &["cat-file", "-s", &spec])?;
    if !size.success() {
        // The usual reason is that the path does not exist at that commit, which is what an added or
        // a deleted file looks like. Anything else — a corrupt object, a bad revision — also lands
        // here, and treating it as absent is the safe direction for a read-only view.
        return Ok(Blob::Missing);
    }
    let bytes: usize = size.stdout_trimmed()?.parse().unwrap_or(usize::MAX);
    if bytes > MAX_FILE_BYTES {
        return Ok(Blob::TooLarge);
    }

    let out = git::run(repo, &["cat-file", "blob", &spec])?;
    if !out.success() {
        return Ok(Blob::Missing);
    }
    match String::from_utf8(out.stdout.clone()) {
        // A NUL byte is git's own test for binary, and it is the right one here: a file that is not
        // valid UTF-8 cannot be shown as text, and pretending otherwise with a lossy conversion
        // would render a diff of characters that are not in the file.
        Ok(text) if !text.contains('\0') => Ok(Blob::Text(text)),
        _ => Ok(Blob::Binary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
            .output()
            .expect("git ran");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        first: String,
        second: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        git_in(&repo, &["init", "-q", "."]);
        std::fs::write(repo.join("kept.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "first"]);
        let first = head(&repo);

        std::fs::write(repo.join("kept.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("added.txt"), "new\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "second"]);
        let second = head(&repo);

        Fixture { _dir: dir, repo, first, second }
    }

    #[test]
    fn reports_both_sides_of_a_modified_file() {
        let f = fixture();
        let set = changes_between(&f.repo, &f.first, &f.second, 50).unwrap();
        let kept = set.files.iter().find(|c| c.path == "kept.txt").expect("kept.txt");
        assert_eq!(kept.old_text.as_deref(), Some("one\ntwo\n"));
        assert_eq!(kept.new_text.as_deref(), Some("one\ntwo\nthree\n"));
        assert_eq!(kept.omitted, None);
    }

    /// An added file has no earlier side, and that has to be `None` rather than an empty string: the
    /// interface draws "new file" from the absence, and an empty old version renders as a change from
    /// a blank file, which is a different claim.
    #[test]
    fn an_added_file_has_no_earlier_version() {
        let f = fixture();
        let set = changes_between(&f.repo, &f.first, &f.second, 50).unwrap();
        let added = set.files.iter().find(|c| c.path == "added.txt").expect("added.txt");
        assert_eq!(added.old_text, None);
        assert_eq!(added.new_text.as_deref(), Some("new\n"));
    }

    #[test]
    fn a_deleted_file_has_no_later_version() {
        let f = fixture();
        let set = changes_between(&f.repo, &f.first, &f.second, 50).unwrap();
        let gone = set.files.iter().find(|c| c.path == "gone.txt").expect("gone.txt");
        assert_eq!(gone.old_text.as_deref(), Some("bye\n"));
        assert_eq!(gone.new_text, None);
    }

    /// Marked, not truncated. A truncated diff looks like a small change, which is the one wrong
    /// impression a review surface must not give.
    #[test]
    fn a_file_that_is_not_text_is_marked_rather_than_mangled() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_in(repo, &["init", "-q", "."]);
        std::fs::write(repo.join("seed.txt"), "x\n").unwrap();
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "first"]);
        let first = head(repo);

        std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "binary"]);
        let second = head(repo);

        let set = changes_between(repo, &first, &second, 50).unwrap();
        let bin = set.files.iter().find(|c| c.path == "blob.bin").expect("blob.bin");
        assert_eq!(bin.omitted.as_deref(), Some("binary"));
        assert_eq!(bin.new_text, None);
    }

    /// Reported rather than dropped: a surface that silently shows the first two files of a five-file
    /// change is showing a different change from the one being merged.
    #[test]
    fn reaching_the_cap_says_how_many_were_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_in(repo, &["init", "-q", "."]);
        std::fs::write(repo.join("seed.txt"), "x\n").unwrap();
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "first"]);
        let first = head(repo);

        for i in 0..5 {
            std::fs::write(repo.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        }
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "many"]);
        let second = head(repo);

        let set = changes_between(repo, &first, &second, 2).unwrap();
        assert_eq!(set.files.len(), 2);
        assert_eq!(set.truncated, 3);
    }

    /// Names git would otherwise quote. The hardening that turns off `core.quotePath` is what makes
    /// this work, and a review surface is where a mis-parsed name shows up as a file that does not
    /// exist.
    #[test]
    fn a_non_ascii_filename_survives() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_in(repo, &["init", "-q", "."]);
        std::fs::write(repo.join("seed.txt"), "x\n").unwrap();
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "first"]);
        let first = head(repo);

        std::fs::write(repo.join("unicöde.txt"), "yes\n").unwrap();
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-q", "-m", "unicode"]);
        let second = head(repo);

        let set = changes_between(repo, &first, &second, 50).unwrap();
        let found = set.files.iter().find(|c| c.path == "unicöde.txt");
        assert!(found.is_some(), "{:?}", set.files.iter().map(|f| &f.path).collect::<Vec<_>>());
        assert_eq!(found.unwrap().new_text.as_deref(), Some("yes\n"));
    }

    #[test]
    fn nothing_changed_is_an_empty_set_rather_than_an_error() {
        let f = fixture();
        let set = changes_between(&f.repo, &f.second, &f.second, 50).unwrap();
        assert!(set.files.is_empty());
        assert_eq!(set.truncated, 0);
    }
}
