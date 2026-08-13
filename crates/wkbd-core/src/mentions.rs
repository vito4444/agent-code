//! Turning "@src/main.rs" into something the agent can actually read.
//!
//! ## Why this is not just string concatenation
//!
//! The obvious implementation pastes the file's text into the prompt and moves on. It works, and
//! it throws away the two things the protocol gives us. An embedded resource keeps its URI, so
//! the agent knows the text it is reading is `src/main.rs` at a path it can edit rather than an
//! anonymous quotation. And a resource link lets an agent that can already read the repository
//! decide for itself how much of a 4 MB file it wants, instead of us spending the context window
//! on its behalf.
//!
//! ## The capability fork
//!
//! Text and resource links are the protocol's baseline: every agent must accept both. Embedded
//! resources need `embeddedContext` and images need `image`. So each mention resolves down one of
//! two paths, and which one is not a preference — sending an unannounced block to an agent that
//! did not advertise it is out of spec, and the agents that tolerate it are the dangerous case,
//! because they drop the block and answer as though the user attached nothing.
//!
//! When we fall back, we record why. "The agent got the path, not the contents" is the difference
//! between an answer that read the file and an answer that guessed, and the user is the only one
//! who can tell that the agent never opened it.
//!
//! ## Reading on the user's behalf
//!
//! Every read here goes through the same [`PathGuard`] as `fs/read_text_file`. The composer is a
//! more tempting bypass than it looks: the request arrives over localhost HTTP naming a path, and
//! a daemon that joined it to the project root with `Path::join` would hand out `/etc/shadow` for
//! a mention of `../../../../etc/shadow`. Resolution happens in the kernel, beneath the root, with
//! symlinks refused — the composer cannot reach anything the agent could not have reached.

use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use wkbd_proto::{Attachment, PromptCapabilities, SentAs};
use wkbd_sec::path_guard::PathGuard;

/// Largest file we will inline into a prompt.
///
/// Much smaller than the 8 MB `fs/read_text_file` allows, because the costs are different. There,
/// the agent asked for the file and pays for it in its own context on purpose. Here the user
/// picked a name off a list, and a 3 MB minified bundle would silently consume the window that
/// the rest of the conversation needs. Past the limit the mention still goes, as a link, which
/// leaves the choice with the party that can make it cheaply.
const MAX_EMBED_BYTES: u64 = 256 * 1024;

/// A file or directory the user picked out of the completion list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Entry {
    /// Relative to the project root, and the only form that crosses the API. An absolute path
    /// from the client would be a path the client chose, which is the thing the guard exists to
    /// stop being load-bearing.
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug)]
pub enum MentionError {
    /// The guard refused, or the file is gone. Named rather than dropped: a prompt that quietly
    /// lost an attachment produces an answer about nothing, and the user has no way to tell that
    /// from the agent ignoring them.
    Unresolvable { path: String, reason: String },
}

impl std::fmt::Display for MentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MentionError::Unresolvable { path, reason } => {
                write!(f, "cannot attach {path}: {reason}")
            }
        }
    }
}

/// One mention, resolved: the block to send and the record to keep.
#[derive(Debug)]
pub struct Resolved {
    pub blocks: Vec<Value>,
    pub attachments: Vec<Attachment>,
}

/// Resolves every mention against the guard and the agent's declared capabilities.
///
/// All-or-nothing on purpose. A partial success would send a prompt whose text refers to files
/// that are not in it.
pub fn resolve(
    guard: &PathGuard,
    root: &Path,
    requested: &[String],
    caps: PromptCapabilities,
) -> Result<Resolved, MentionError> {
    let mut blocks = Vec::new();
    let mut attachments = Vec::new();

    for rel in requested {
        let (block, attachment) = resolve_one(guard, root, rel, caps)?;
        blocks.push(block);
        attachments.push(attachment);
    }

    Ok(Resolved { blocks, attachments })
}

fn resolve_one(
    guard: &PathGuard,
    root: &Path,
    rel: &str,
    caps: PromptCapabilities,
) -> Result<(Value, Attachment), MentionError> {
    let joined = root.join(rel);

    // `resolve_for_display` runs the same beneath-the-root resolution as a read, so an escape is
    // refused here rather than after we have already decided what kind of block to build.
    let resolved = guard.resolve_for_display(&joined).map_err(|e| MentionError::Unresolvable {
        path: rel.to_string(),
        reason: e.audit_kind().to_string(),
    })?;
    let uri = file_uri(&resolved);

    let meta = std::fs::symlink_metadata(&resolved).map_err(|e| MentionError::Unresolvable {
        path: rel.to_string(),
        reason: e.to_string(),
    })?;

    // A directory is always a link. There is no "contents" to embed, and an agent that can read
    // the repository can list it far more cheaply than we can serialize it.
    if meta.is_dir() {
        return Ok((
            link_block(&uri, rel, None),
            Attachment {
                uri,
                name: rel.to_string(),
                sent_as: SentAs::Link,
                bytes: None,
                degraded: Some("directory".into()),
            },
        ));
    }

    let size = meta.len();
    let mime = mime_for(rel);
    let is_image = mime.starts_with("image/");

    let degrade = |reason: &str| {
        (
            link_block(&uri, rel, Some(size)),
            Attachment {
                uri: uri.clone(),
                name: rel.to_string(),
                sent_as: SentAs::Link,
                bytes: Some(size),
                degraded: Some(reason.to_string()),
            },
        )
    };

    if size > MAX_EMBED_BYTES {
        return Ok(degrade("too-large"));
    }
    if is_image && !caps.image {
        return Ok(degrade("agent-cannot-embed"));
    }
    if !is_image && !caps.embedded_context {
        return Ok(degrade("agent-cannot-embed"));
    }

    let mut file = guard.open_read(&joined).map_err(|e| MentionError::Unresolvable {
        path: rel.to_string(),
        reason: e.audit_kind().to_string(),
    })?;
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes).map_err(|e| MentionError::Unresolvable {
        path: rel.to_string(),
        reason: e.to_string(),
    })?;

    if is_image {
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok((
            json!({ "type": "image", "mimeType": mime, "data": data, "uri": uri }),
            Attachment {
                uri,
                name: rel.to_string(),
                sent_as: SentAs::Image,
                bytes: Some(size),
                degraded: None,
            },
        ));
    }

    // Binary that is not an image we can send: a resource's `text` field must be text, and
    // lossy-converting it would hand the agent a file full of replacement characters that it
    // has no way to know is not the real content.
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(degrade("not-text"));
    };

    Ok((
        json!({
            "type": "resource",
            "resource": { "uri": uri, "mimeType": mime, "text": text },
        }),
        Attachment {
            uri,
            name: rel.to_string(),
            sent_as: SentAs::Embedded,
            bytes: Some(size),
            degraded: None,
        },
    ))
}

fn link_block(uri: &str, name: &str, size: Option<u64>) -> Value {
    let mut v = json!({ "type": "resource_link", "uri": uri, "name": name });
    if let Some(size) = size {
        v["size"] = json!(size);
    }
    v
}

/// `file:///abs/path`, percent-encoding the characters that would otherwise end the path.
///
/// Not a general URI encoder: the agent has to be able to turn this back into a path, and
/// over-encoding is as wrong as under-encoding. Only the delimiters matter.
fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            c => out.push(c),
        }
    }
    out
}

fn mime_for(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let m = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "ts" | "tsx" => "text/typescript",
        "js" | "jsx" => "text/javascript",
        "json" => "application/json",
        "toml" => "text/x-toml",
        "yaml" | "yml" => "text/x-yaml",
        "md" => "text/markdown",
        "html" => "text/html",
        "css" => "text/css",
        "sh" => "text/x-shellscript",
        "sql" => "text/x-sql",
        "go" => "text/x-go",
        _ => "text/plain",
    };
    m.to_string()
}

/// Files under the project root matching `query`, for the completion list.
///
/// Respects `.gitignore`, which is not a nicety: in a repository with a `node_modules` or a
/// `target`, an unfiltered walk returns tens of thousands of build artifacts and the first
/// hundred matches for any query are all generated files. The user's own ignore file is the
/// best available statement of what they consider part of the project.
///
/// Symlinks are not followed. The guard would refuse to read through one anyway, so listing it
/// would only offer a completion that fails on send.
pub fn search(root: &Path, query: &str, limit: usize) -> Vec<Entry> {
    let needle = query.to_ascii_lowercase();
    let mut scored: Vec<(u32, Entry)> = Vec::new();

    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_global(false)
        // Without this the ignore file is only consulted when the root is itself a repository —
        // and a session opened on a subdirectory of one, or on a project that keeps a
        // `.gitignore` without being a repository yet, would list every build artifact it has.
        .require_git(false)
        .max_depth(Some(12))
        .build();

    // A bound on work rather than on results: a query matching nothing in a very large repository
    // must not walk it entirely while the user waits between keystrokes.
    let mut examined = 0usize;
    for entry in walker.flatten() {
        examined += 1;
        if examined > 20_000 {
            break;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().to_string();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());

        let Some(score) = score(&needle, &name, &rel) else { continue };
        scored.push((score, Entry { path: rel, name, is_dir }));
    }

    // Sort by score, then by path length: with equal relevance the shallower file is nearly
    // always the one meant, and stable ordering keeps the list from reshuffling under the cursor.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.len().cmp(&b.1.path.len())));
    scored.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// Higher is better. `None` means no match.
fn score(needle: &str, name: &str, rel: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(1);
    }
    let name_l = name.to_ascii_lowercase();
    let rel_l = rel.to_ascii_lowercase();
    if name_l == needle {
        Some(100)
    } else if name_l.starts_with(needle) {
        Some(80)
    } else if name_l.contains(needle) {
        Some(60)
    } else if rel_l.contains(needle) {
        Some(40)
    } else {
        None
    }
}

/// Extracts the paths the user typed as `@…` from the message.
///
/// The composer sends its own list, so this exists for the case the list and the text disagree:
/// a user who types `@src/main.rs` by hand, or edits a path a completion inserted. Taking the
/// union means a hand-typed mention still attaches, which is what someone who typed it expects.
pub fn scan_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // An `@` mid-word is an email address or a decorator, not a mention.
        if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = rest[..end].trim_end_matches([',', '.', ';', ':', ')']);
        if !candidate.is_empty() && !candidate.contains('@') {
            out.push(candidate.to_string());
        }
        i += 1 + end;
    }
    out
}

/// The absolute root for one session, as a `PathBuf` the guard can be built around.
pub fn root_of(project_root: &str) -> PathBuf {
    PathBuf::from(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(image: bool, embedded: bool) -> PromptCapabilities {
        PromptCapabilities { image, audio: false, embedded_context: embedded }
    }

    fn fixture() -> (tempfile::TempDir, PathGuard) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "# hi\n").unwrap();
        // A real PNG header, so the extension and the bytes agree.
        std::fs::write(root.join("shot.png"), [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
            .unwrap();
        let guard = PathGuard::new(vec![root.clone()]).unwrap();
        (dir, guard)
    }

    #[test]
    fn an_agent_that_can_embed_gets_the_contents() {
        let (dir, guard) = fixture();
        let r =
            resolve(&guard, dir.path(), &["src/main.rs".into()], caps(false, true)).unwrap();

        assert_eq!(r.blocks[0]["type"], "resource");
        assert_eq!(r.blocks[0]["resource"]["text"], "fn main() {}\n");
        assert!(r.blocks[0]["resource"]["uri"]
            .as_str()
            .unwrap()
            .ends_with("/src/main.rs"));
        assert_eq!(r.attachments[0].sent_as, SentAs::Embedded);
        assert_eq!(r.attachments[0].degraded, None);
    }

    /// The whole point of reading the handshake. An agent that never advertised
    /// `embeddedContext` must not be sent one, and the user must be able to see that the agent
    /// got a path rather than the file.
    #[test]
    fn an_agent_that_cannot_embed_gets_a_link_and_the_user_is_told() {
        let (dir, guard) = fixture();
        let r =
            resolve(&guard, dir.path(), &["src/main.rs".into()], caps(false, false)).unwrap();

        assert_eq!(r.blocks[0]["type"], "resource_link");
        assert_eq!(r.blocks[0]["name"], "src/main.rs");
        assert!(r.blocks[0].get("text").is_none(), "a link must not carry the contents");
        assert_eq!(r.attachments[0].sent_as, SentAs::Link);
        assert_eq!(r.attachments[0].degraded.as_deref(), Some("agent-cannot-embed"));
    }

    #[test]
    fn an_image_goes_as_an_image_only_when_the_agent_takes_images() {
        let (dir, guard) = fixture();

        let yes = resolve(&guard, dir.path(), &["shot.png".into()], caps(true, true)).unwrap();
        assert_eq!(yes.blocks[0]["type"], "image");
        assert_eq!(yes.blocks[0]["mimeType"], "image/png");
        assert_eq!(yes.blocks[0]["data"], "iVBORw0KGgo=");
        assert_eq!(yes.attachments[0].sent_as, SentAs::Image);

        // Embedded context is on, but that does not make an image embeddable: a PNG in a
        // resource's `text` field is not text, and the agent asked for neither.
        let no = resolve(&guard, dir.path(), &["shot.png".into()], caps(false, true)).unwrap();
        assert_eq!(no.blocks[0]["type"], "resource_link");
        assert_eq!(no.attachments[0].degraded.as_deref(), Some("agent-cannot-embed"));
    }

    /// The composer must not become the way around the boundary. The path arrives over HTTP as
    /// a string, and joining it to the root is not the same as resolving beneath the root.
    #[test]
    fn a_path_escaping_the_root_is_refused_rather_than_attached() {
        let (dir, guard) = fixture();
        let err = resolve(
            &guard,
            dir.path(),
            &["../../../../etc/passwd".into()],
            caps(true, true),
        )
        .unwrap_err();
        let MentionError::Unresolvable { path, .. } = err;
        assert_eq!(path, "../../../../etc/passwd");
    }

    #[test]
    fn a_symlink_out_of_the_root_is_refused_too() {
        let (dir, guard) = fixture();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("escape.txt")).unwrap();
        let err =
            resolve(&guard, dir.path(), &["escape.txt".into()], caps(true, true)).unwrap_err();
        let MentionError::Unresolvable { path, .. } = err;
        assert_eq!(path, "escape.txt");
    }

    #[test]
    fn a_directory_is_always_a_link() {
        let (dir, guard) = fixture();
        let r = resolve(&guard, dir.path(), &["src".into()], caps(true, true)).unwrap();
        assert_eq!(r.blocks[0]["type"], "resource_link");
        assert_eq!(r.attachments[0].degraded.as_deref(), Some("directory"));
    }

    #[test]
    fn something_too_large_to_inline_still_attaches_as_a_link() {
        let (dir, guard) = fixture();
        let big = "x".repeat(MAX_EMBED_BYTES as usize + 1);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();

        let r = resolve(&guard, dir.path(), &["big.txt".into()], caps(true, true)).unwrap();
        assert_eq!(r.blocks[0]["type"], "resource_link");
        assert_eq!(r.attachments[0].degraded.as_deref(), Some("too-large"));
        assert_eq!(r.attachments[0].bytes, Some(MAX_EMBED_BYTES + 1));
    }

    #[test]
    fn binary_that_is_not_an_image_is_linked_rather_than_mangled() {
        let (dir, guard) = fixture();
        std::fs::write(dir.path().join("a.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let r = resolve(&guard, dir.path(), &["a.bin".into()], caps(true, true)).unwrap();
        assert_eq!(r.attachments[0].degraded.as_deref(), Some("not-text"));
    }

    #[test]
    fn ordering_survives_resolution() {
        let (dir, guard) = fixture();
        let r = resolve(
            &guard,
            dir.path(),
            &["README.md".into(), "src/main.rs".into()],
            caps(true, true),
        )
        .unwrap();
        let names: Vec<&str> = r.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["README.md", "src/main.rs"]);
    }

    #[test]
    fn search_finds_by_name_and_prefers_the_closer_match() {
        let (dir, _guard) = fixture();
        let hits = search(dir.path(), "main", 10);
        assert_eq!(hits[0].path, "src/main.rs");
    }

    #[test]
    fn search_skips_what_the_repository_ignores() {
        let (dir, _guard) = fixture();
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/main.rs"), "generated").unwrap();

        let hits = search(dir.path(), "main", 10);
        assert!(
            hits.iter().all(|h| !h.path.starts_with("build/")),
            "ignored files must not be offered: {hits:?}"
        );
    }

    #[test]
    fn scan_text_picks_up_hand_typed_paths_but_not_email_addresses() {
        assert_eq!(scan_text("look at @src/main.rs please"), ["src/main.rs"]);
        assert_eq!(scan_text("mail me@example.com"), Vec::<String>::new());
        assert_eq!(scan_text("check @a.rs, then @b.rs."), ["a.rs", "b.rs"]);
        assert_eq!(scan_text("no mentions here"), Vec::<String>::new());
    }

    /// Conformance: what we put on the wire has to be what the specification says, checked
    /// against the official types rather than against our reading of the prose.
    #[test]
    fn the_blocks_we_build_satisfy_the_official_schema() {
        use agent_client_protocol::schema::v1;

        let (dir, guard) = fixture();
        let r = resolve(
            &guard,
            dir.path(),
            &["src/main.rs".into(), "shot.png".into(), "src".into()],
            caps(true, true),
        )
        .unwrap();

        for block in &r.blocks {
            serde_json::from_value::<v1::ContentBlock>(block.clone()).unwrap_or_else(|e| {
                panic!("block {block} is not a valid ContentBlock: {e}")
            });
        }

        // And in the request that carries them, since a block that parses alone could still be
        // rejected in context.
        let prompt = serde_json::json!({ "sessionId": "s-1", "prompt": r.blocks });
        serde_json::from_value::<v1::PromptRequest>(prompt)
            .expect("a prompt carrying attachments must satisfy the official schema");
    }
}
