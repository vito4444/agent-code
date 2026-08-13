//! Every case here builds real directories, real files and real symlinks under a
//! `tempfile::TempDir` and calls the real syscalls. A boundary check tested against
//! strings is a string test, and string tests are exactly what shipped in the four CVEs
//! this module exists to avoid.
//!
//! Each case runs once per backend reported by [`PathGuard::available_backends`], so the
//! `openat2` path and the fallback walk are held to the same standard on a kernel that
//! has both.

use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use wkbd_sec::path_guard::{Backend, GuardError, PathGuard};

/// TempDir hands back a path that may itself traverse a symlink (`/tmp` is one on macOS,
/// and on some CI images on Linux). Canonicalising up front keeps the tests about the
/// code under test rather than about the host's `/tmp`.
fn workspace() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    (tmp, base)
}

fn guard(backend: Backend, roots: &[&Path]) -> PathGuard {
    PathGuard::with_backend(roots.iter().map(|p| p.to_path_buf()).collect(), backend)
        .expect("guard construction")
}

fn for_each_backend(mut case: impl FnMut(Backend)) {
    let backends = PathGuard::available_backends();
    assert!(!backends.is_empty(), "no usable backend on this platform");
    for backend in backends {
        eprintln!("--- backend: {backend:?}");
        case(backend);
    }
}

fn read_all(guard: &PathGuard, path: &Path) -> Result<String, GuardError> {
    let mut file = guard.open_read(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).unwrap();
    Ok(buf)
}

#[test]
fn openat2_is_the_backend_in_use_on_this_kernel() {
    // Not a policy assertion, a deployment one: if this fails the daemon is silently
    // running on the weaker resolver, either because the kernel predates 5.6 or because a
    // seccomp profile is answering EPERM.
    assert!(
        PathGuard::available_backends().contains(&Backend::Openat2),
        "openat2(2) unavailable; the fallback walk is in use"
    );
}

#[test]
fn regular_file_inside_a_root_reads_and_writes() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        std::fs::write(base.join("work/notes.txt"), b"before").unwrap();
        let g = guard(backend, &[&base.join("work")]);

        assert_eq!(
            read_all(&g, &base.join("work/notes.txt")).unwrap(),
            "before"
        );

        let mut out = g.open_write_create(&base.join("work/notes.txt")).unwrap();
        out.write_all(b"after").unwrap();
        drop(out);

        assert_eq!(
            std::fs::read_to_string(base.join("work/notes.txt")).unwrap(),
            "after",
            "open_write_create must replace the contents, not append to them"
        );
    });
}

#[test]
fn nested_directories_inside_a_root_are_reachable() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/src/deep")).unwrap();
        std::fs::write(base.join("work/src/deep/f.rs"), b"fn main() {}").unwrap();
        let g = guard(backend, &[&base.join("work")]);

        assert_eq!(
            read_all(&g, &base.join("work/src/deep/f.rs")).unwrap(),
            "fn main() {}"
        );
    });
}

#[test]
fn parent_traversal_is_refused() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        std::fs::write(base.join("secret.txt"), b"secret").unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, &base.join("work/../secret.txt")).unwrap_err();
        assert_eq!(err.audit_kind(), "parent-traversal", "{err}");
        assert!(err.is_boundary_violation());

        // ..and the same request phrased so that it never touches the root prefix at all.
        let err = read_all(&g, &base.join("secret.txt")).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");
    });
}

#[test]
fn a_relative_path_is_refused_rather_than_joined() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, Path::new("notes.txt")).unwrap_err();
        assert_eq!(err.audit_kind(), "not-absolute", "{err}");
    });
}

#[test]
fn symlink_in_root_pointing_outside_is_refused() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        std::fs::write(base.join("outside.txt"), b"stolen").unwrap();
        symlink(base.join("outside.txt"), base.join("work/link.txt")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, &base.join("work/link.txt")).unwrap_err();
        assert_eq!(err.audit_kind(), "symlink-encountered", "{err}");

        // Writing through the link must not create or clobber the target either.
        let err = g
            .open_write_create(&base.join("work/link.txt"))
            .unwrap_err();
        assert_eq!(err.audit_kind(), "symlink-encountered", "{err}");
        assert_eq!(
            std::fs::read_to_string(base.join("outside.txt")).unwrap(),
            "stolen",
            "the outside file was modified through a symlink"
        );
    });
}

#[test]
fn symlink_as_an_intermediate_component_is_refused() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/a")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::fs::write(base.join("elsewhere/b"), b"stolen").unwrap();
        symlink(base.join("elsewhere"), base.join("work/a/link")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, &base.join("work/a/link/b")).unwrap_err();
        assert_eq!(err.audit_kind(), "symlink-encountered", "{err}");
    });
}

#[test]
fn symlink_pointing_back_inside_the_root_is_also_refused() {
    // Documented policy, not an oversight: allowing this requires resolving the link and
    // re-checking the result, and the gap between those two steps is the race that
    // RESOLVE_NO_SYMLINKS removes. See the module docs.
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/real")).unwrap();
        std::fs::write(base.join("work/real/f.txt"), b"legit").unwrap();
        symlink(base.join("work/real"), base.join("work/alias")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        // The real path still works.
        assert_eq!(
            read_all(&g, &base.join("work/real/f.txt")).unwrap(),
            "legit"
        );
        // The aliased path does not.
        let err = read_all(&g, &base.join("work/alias/f.txt")).unwrap_err();
        assert_eq!(err.audit_kind(), "symlink-encountered", "{err}");
    });
}

#[test]
fn prefix_confusion_is_refused() {
    // CVE-2025-53110 exactly: `/x/allowed_evil` passes a string `starts_with` against
    // root `/x/allowed`, and fails a component-wise comparison.
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        let x = base.join("x");
        std::fs::create_dir_all(x.join("allowed")).unwrap();
        std::fs::create_dir_all(x.join("allowed_evil")).unwrap();
        std::fs::write(x.join("allowed_evil/f"), b"stolen").unwrap();
        let g = guard(backend, &[&x.join("allowed")]);

        let err = read_all(&g, &x.join("allowed_evil/f")).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");

        let err = g.open_write_create(&x.join("allowed_evil/f")).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");
        assert!(
            !x.join("allowed_evil/f2").exists(),
            "nothing may be created outside the root"
        );
    });
}

#[test]
fn missing_file_inside_a_root_is_created() {
    // ACP: "The Client MUST create the file if it doesn't exist." Not-there is the normal
    // path here, and it must stay inside the same resolution rules as everything else.
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/sub")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let target = base.join("work/sub/new.txt");
        assert!(!target.exists());
        let mut file = g.open_write_create(&target).unwrap();
        file.write_all(b"created").unwrap();
        drop(file);

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "created");
    });
}

#[test]
fn missing_file_outside_a_root_is_not_created() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let outside = base.join("new.txt");
        let err = g.open_write_create(&outside).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");
        assert!(!outside.exists());

        let traversal = base.join("work/../new2.txt");
        let err = g.open_write_create(&traversal).unwrap_err();
        assert_eq!(err.audit_kind(), "parent-traversal", "{err}");
        assert!(!base.join("new2.txt").exists());
    });
}

#[test]
fn missing_parent_directory_is_not_created_implicitly() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = g
            .open_write_create(&base.join("work/nope/f.txt"))
            .unwrap_err();
        assert!(err.is_not_found(), "{err} ({})", err.audit_kind());
    });
}

#[test]
fn a_directory_is_not_a_readable_file() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/sub")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, &base.join("work/sub")).unwrap_err();
        assert_eq!(err.audit_kind(), "not-a-regular-file", "{err}");
    });
}

#[test]
fn a_file_used_as_a_directory_is_reported_as_such() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("work")).unwrap();
        std::fs::write(base.join("work/f.txt"), b"x").unwrap();
        let g = guard(backend, &[&base.join("work")]);

        let err = read_all(&g, &base.join("work/f.txt/inner")).unwrap_err();
        assert_eq!(err.audit_kind(), "not-a-directory", "{err}");
    });
}

#[test]
fn several_roots_are_each_honoured() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir(base.join("one")).unwrap();
        std::fs::create_dir(base.join("two")).unwrap();
        std::fs::create_dir(base.join("three")).unwrap();
        std::fs::write(base.join("one/a"), b"a").unwrap();
        std::fs::write(base.join("two/b"), b"b").unwrap();
        std::fs::write(base.join("three/c"), b"c").unwrap();
        let g = guard(backend, &[&base.join("one"), &base.join("two")]);

        assert_eq!(read_all(&g, &base.join("one/a")).unwrap(), "a");
        assert_eq!(read_all(&g, &base.join("two/b")).unwrap(), "b");
        let err = read_all(&g, &base.join("three/c")).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");
    });
}

#[test]
fn resolve_for_display_reports_paths_and_refuses_the_same_requests() {
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        std::fs::create_dir_all(base.join("work/sub")).unwrap();
        std::fs::write(base.join("work/sub/f.txt"), b"x").unwrap();
        symlink(base.join("work/sub"), base.join("work/alias")).unwrap();
        let g = guard(backend, &[&base.join("work")]);

        assert_eq!(
            g.resolve_for_display(&base.join("work/sub/f.txt")).unwrap(),
            base.join("work/sub/f.txt")
        );
        // A leaf that does not exist yet still resolves: the parent is what has to pass.
        assert_eq!(
            g.resolve_for_display(&base.join("work/sub/new.txt"))
                .unwrap(),
            base.join("work/sub/new.txt")
        );
        let err = g
            .resolve_for_display(&base.join("work/alias/f.txt"))
            .unwrap_err();
        assert_eq!(err.audit_kind(), "symlink-encountered", "{err}");
        let err = g.resolve_for_display(&base.join("elsewhere")).unwrap_err();
        assert_eq!(err.audit_kind(), "outside-root", "{err}");
    });
}

#[test]
fn a_root_replaced_after_construction_does_not_move_the_boundary() {
    // The root descriptor is opened once and held. Swapping the directory the root *name*
    // refers to must not hand the agent a new boundary.
    for_each_backend(|backend| {
        let (_tmp, base) = workspace();
        let root = base.join("work");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("f.txt"), b"original").unwrap();
        let g = guard(backend, &[&root]);

        std::fs::create_dir(base.join("attacker")).unwrap();
        std::fs::write(base.join("attacker/f.txt"), b"substituted").unwrap();
        std::fs::rename(&root, base.join("moved")).unwrap();
        std::fs::rename(base.join("attacker"), &root).unwrap();

        assert_eq!(
            read_all(&g, &root.join("f.txt")).unwrap(),
            "original",
            "the guard followed the name instead of the descriptor it validated"
        );
    });
}

#[test]
fn both_backends_agree_on_every_refusal() {
    // A fallback that refuses different things from the primary is a fallback that will
    // eventually be the one deciding, in production, differently.
    let backends = PathGuard::available_backends();
    if backends.len() < 2 {
        eprintln!("only one backend available; cross-checking skipped");
        return;
    }
    let (_tmp, base) = workspace();
    std::fs::create_dir_all(base.join("work/a")).unwrap();
    std::fs::create_dir_all(base.join("work/real")).unwrap();
    std::fs::write(base.join("work/real/f.txt"), b"legit").unwrap();
    std::fs::create_dir_all(base.join("elsewhere")).unwrap();
    std::fs::write(base.join("elsewhere/b"), b"stolen").unwrap();
    symlink(base.join("elsewhere"), base.join("work/a/link")).unwrap();
    symlink(base.join("elsewhere/b"), base.join("work/leaf")).unwrap();
    symlink(base.join("work/real"), base.join("work/alias")).unwrap();

    let requests = [
        base.join("work/real/f.txt"),
        base.join("work/a/link/b"),
        base.join("work/leaf"),
        base.join("work/alias/f.txt"),
        base.join("work/../elsewhere/b"),
        base.join("elsewhere/b"),
        base.join("work/missing.txt"),
        base.join("work/real"),
        PathBuf::from("relative.txt"),
    ];

    let outcomes: Vec<Vec<String>> = backends
        .iter()
        .map(|backend| {
            let g = guard(*backend, &[&base.join("work")]);
            requests
                .iter()
                .map(|r| match g.open_read(r) {
                    Ok(_) => "ok".to_string(),
                    Err(err) => err.audit_kind().to_string(),
                })
                .collect()
        })
        .collect();

    for (i, request) in requests.iter().enumerate() {
        let first = &outcomes[0][i];
        for (b, outcome) in backends.iter().zip(&outcomes) {
            assert_eq!(
                &outcome[i],
                first,
                "{b:?} disagrees about {}",
                request.display()
            );
        }
    }
}
