//! Filesystem boundary enforcement for paths that arrive from an agent.
//!
//! # Why this module is written the way it is
//!
//! ACP's `fs/read_text_file` and `fs/write_text_file` are executed by *us*, not by the
//! agent: the agent sends an absolute `path` and the client performs the real disk I/O
//! with the client's own privileges. The protocol says the client MUST create the file if
//! it does not exist, and it says nothing whatsoever about restricting where that path
//! may point. The boundary is entirely ours to enforce.
//!
//! The same bug has now shipped four separate times — CVE-2026-50549 (Cursor),
//! CVE-2025-53110 and CVE-2025-53109 (Anthropic Filesystem MCP Server), CVE-2025-67366 —
//! and the shape is identical every time: the code canonicalises the path, and when
//! canonicalisation fails (or is skipped for a not-yet-existing file) it falls back to
//! comparing strings. Two consequences follow, and both are load-bearing here:
//!
//! 1. **Fail closed.** A resolution failure is a denial. There is no fallback path that
//!    compares strings, because a string comparison cannot see a symlink.
//! 2. **Decide and act on the same object.** Every method returns an open file
//!    descriptor, never a validated path for the caller to open later. A validated path
//!    is a TOCTOU bug waiting for someone to `rename(2)` a directory between the check
//!    and the open.
//!
//! CVE-2025-53110 in particular is prefix confusion: root `/x/allowed` versus request
//! `/x/allowed_evil/f`, which passes `starts_with` on strings and fails it on path
//! components. Matching here is component-wise, and the kernel enforces containment again
//! underneath that.
//!
//! # Symlink policy
//!
//! **Every symlink is rejected, including one that points back inside a root.** This is a
//! deliberate loss of functionality. Permitting an inside-root symlink requires resolving
//! it and then re-checking the result against the roots, and between that check and the
//! open, the link can be repointed — the exact race `RESOLVE_NO_SYMLINKS` exists to
//! remove. A repository that genuinely needs a symlink traversed can have the real
//! directory added as an additional root, which is an explicit, auditable decision rather
//! than an implicit one made by whatever the filesystem happened to contain.
//!
//! # Backends
//!
//! [`Backend::Openat2`] is preferred: `openat2(2)` with
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` makes the kernel decide
//! containment during resolution itself, so there is no window at all. It needs Linux
//! 5.6+, and Docker's default seccomp profile used to reject it with `EPERM`, so
//! availability is probed at runtime and [`Backend::Walk`] takes over when it is missing.
//!
//! [`Backend::Walk`] opens each component in turn from a directory descriptor with
//! `O_NOFOLLOW`, checks the type of what it got, and never touches a string path again
//! after the first component. It enforces the same policy with a slightly larger attack
//! surface (each step is a separate syscall), which is why it is the fallback and not the
//! default.
//!
//! Mount points are *not* blocked (`RESOLVE_NO_XDEV` is not set). A bind mount inside a
//! root that points outside it would escape, but creating one requires privileges the
//! agent does not have, and setting the flag would break repositories whose submodules or
//! build directories live on another filesystem.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// Which resolver a [`PathGuard`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// `openat2(2)` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`.
    Openat2,
    /// Component-by-component `openat(2)` walk with `O_NOFOLLOW`.
    Walk,
}

/// Why a request was refused.
///
/// The variants are separated so an audit log can distinguish "the agent asked for
/// something outside its worktree" (interesting) from "the file is not there" (routine).
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("no roots configured")]
    NoRoots,

    #[error("path must be absolute: {requested}")]
    NotAbsolute { requested: PathBuf },

    #[error("path contains a `..` component: {requested}")]
    ParentTraversal { requested: PathBuf },

    #[error("path is outside every root: {requested}")]
    OutsideRoot { requested: PathBuf },

    #[error("symlink encountered at {at} while resolving {requested}")]
    SymlinkEncountered { requested: PathBuf, at: PathBuf },

    #[error("{at} is not a directory, while resolving {requested}")]
    NotADirectory { requested: PathBuf, at: PathBuf },

    #[error("{requested} is not a regular file")]
    NotARegularFile { requested: PathBuf },

    #[error("path cannot be represented as a C string: {requested}")]
    MalformedPath { requested: PathBuf },

    #[error("io error while resolving {requested}: {source}")]
    Io {
        requested: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("requested backend is unavailable on this kernel")]
    BackendUnavailable,

    #[error("path guard has no implementation for this platform")]
    UnsupportedPlatform,
}

impl GuardError {
    /// Stable identifier for audit logs, so the log schema does not change when a message
    /// is reworded.
    pub fn audit_kind(&self) -> &'static str {
        match self {
            GuardError::NoRoots => "no-roots",
            GuardError::NotAbsolute { .. } => "not-absolute",
            GuardError::ParentTraversal { .. } => "parent-traversal",
            GuardError::OutsideRoot { .. } => "outside-root",
            GuardError::SymlinkEncountered { .. } => "symlink-encountered",
            GuardError::NotADirectory { .. } => "not-a-directory",
            GuardError::NotARegularFile { .. } => "not-a-regular-file",
            GuardError::MalformedPath { .. } => "malformed-path",
            GuardError::Io { .. } => "io-error",
            GuardError::BackendUnavailable => "backend-unavailable",
            GuardError::UnsupportedPlatform => "unsupported-platform",
        }
    }

    /// True when the refusal was only "it is not there". Callers that implement the
    /// protocol's create-on-write behaviour need to tell this apart from a real denial.
    pub fn is_not_found(&self) -> bool {
        matches!(self, GuardError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }

    /// True when the request was refused for a boundary reason rather than an
    /// environmental one. These are the ones worth alerting on.
    pub fn is_boundary_violation(&self) -> bool {
        matches!(
            self,
            GuardError::NotAbsolute { .. }
                | GuardError::ParentTraversal { .. }
                | GuardError::OutsideRoot { .. }
                | GuardError::SymlinkEncountered { .. }
                | GuardError::MalformedPath { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Read,
    WriteCreate,
    /// Resolve only: the descriptor is opened purely to prove the path is legal, and may
    /// refer to a directory.
    Probe,
}

// ---------------------------------------------------------------------------------------
// Unix implementation
// ---------------------------------------------------------------------------------------

#[cfg(unix)]
mod sys {
    use super::*;

    // `open_how` is not in libc and glibc ships no wrapper for openat2(2), so both are
    // declared here. The struct is extensible: the kernel compares the caller's size
    // against its own and returns E2BIG if any field it does not know about is non-zero,
    // which is why it must be zero-initialised rather than left as uninitialised memory.
    #[repr(C)]
    #[derive(Debug, Default, Clone, Copy)]
    pub(super) struct OpenHow {
        pub flags: u64,
        pub mode: u64,
        pub resolve: u64,
    }

    pub(super) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    pub(super) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    pub(super) const RESOLVE_BENEATH: u64 = 0x08;

    /// The whole point of the openat2 backend. `RESOLVE_BENEATH` refuses any resolution
    /// that leaves the starting descriptor (including absolute paths and `..` escapes),
    /// `RESOLVE_NO_SYMLINKS` refuses every symlink rather than resolving it, and
    /// `RESOLVE_NO_MAGICLINKS` refuses the `/proc/*/fd/*` style links that are not
    /// symlinks and would otherwise slip past the first two.
    pub(super) const STRICT_RESOLVE: u64 =
        RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;

    /// Directory-walk step flags. `O_PATH` gets a descriptor without opening the object,
    /// which matters because the intermediate component could be a fifo or a device that
    /// would block or have side effects on open. Combined with `O_NOFOLLOW` a symlink
    /// yields a descriptor to the *link itself*, which is then rejected by `fstat`.
    #[cfg(target_os = "linux")]
    pub(super) const WALK_FLAGS: libc::c_int = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    /// Without `O_PATH` the next best thing is a directory-only open; a symlink then
    /// fails with `ELOOP` at the syscall instead of at the `fstat`.
    #[cfg(not(target_os = "linux"))]
    pub(super) const WALK_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    #[cfg(target_os = "linux")]
    pub(super) unsafe fn openat2(
        dirfd: RawFd,
        path: &CStr,
        flags: libc::c_int,
        mode: u32,
        resolve: u64,
    ) -> io::Result<OwnedFd> {
        let how = OpenHow {
            flags: flags as u64,
            // The kernel rejects a non-zero mode when O_CREAT/O_TMPFILE are absent, so
            // this cannot simply always be passed through.
            mode: if flags & libc::O_CREAT != 0 {
                mode as u64
            } else {
                0
            },
            resolve,
        };
        let rc = libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(rc as RawFd))
    }

    /// Whether this kernel implements `openat2(2)`.
    ///
    /// `ENOSYS` is the pre-5.6 answer. `EPERM` is the seccomp answer: Docker's default
    /// profile denied unknown syscalls for a long while after openat2 landed, and a
    /// denial there must degrade to the walk backend rather than break the daemon.
    #[cfg(target_os = "linux")]
    pub(super) fn openat2_available() -> bool {
        use std::sync::OnceLock;
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let probe = unsafe {
                openat2(
                    libc::AT_FDCWD,
                    c"/",
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    0,
                    0,
                )
            };
            match probe {
                Ok(_) => true,
                Err(err) => !matches!(
                    err.raw_os_error(),
                    Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL)
                ),
            }
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn openat2_available() -> bool {
        false
    }

    pub(super) fn openat(
        dirfd: RawFd,
        name: &CStr,
        flags: libc::c_int,
        mode: u32,
    ) -> io::Result<OwnedFd> {
        let rc = unsafe { libc::openat(dirfd, name.as_ptr(), flags, mode as libc::c_uint) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(rc) })
    }

    pub(super) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    pub(super) fn is_dir(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub(super) fn is_symlink(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub(super) fn is_regular(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFREG
    }

    /// `O_NONBLOCK` is set on every open so that a fifo planted where a regular file was
    /// expected cannot park the daemon thread forever; it is cleared once `fstat` has
    /// confirmed a regular file, where the flag means nothing anyway.
    pub(super) fn clear_nonblock(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn ftruncate(fd: RawFd) -> io::Result<()> {
        if unsafe { libc::ftruncate(fd, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn cstring(name: &OsStr) -> Option<CString> {
        CString::new(name.as_bytes()).ok()
    }
}

#[cfg(unix)]
struct Root {
    /// The path as configured, kept so a request phrased in the caller's own terms still
    /// matches when the configured path differs from its canonical form.
    display: PathBuf,
    canonical: PathBuf,
    /// Held open for the lifetime of the guard. If the root directory is renamed out from
    /// under us the descriptor keeps pointing at the original inode, which is the safe
    /// answer: an attacker cannot substitute a different directory for the root.
    dir: OwnedFd,
}

/// Decides whether a path is inside the boundary, and returns an open descriptor rather
/// than a path so the decision cannot be invalidated before it is used.
#[cfg(unix)]
pub struct PathGuard {
    roots: Vec<Root>,
    backend: Backend,
}

#[cfg(unix)]
impl std::fmt::Debug for PathGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathGuard")
            .field("backend", &self.backend)
            .field(
                "roots",
                &self.roots.iter().map(|r| &r.canonical).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(unix)]
impl PathGuard {
    /// Uses the strongest backend this kernel supports.
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, GuardError> {
        let backend = if sys::openat2_available() {
            Backend::Openat2
        } else {
            Backend::Walk
        };
        Self::with_backend(roots, backend)
    }

    /// Pins the backend. Passing [`Backend::Openat2`] on a kernel without it is an error
    /// rather than a silent downgrade, so a deployment that means to depend on the
    /// kernel-side check finds out at startup.
    pub fn with_backend(roots: Vec<PathBuf>, backend: Backend) -> Result<Self, GuardError> {
        if roots.is_empty() {
            return Err(GuardError::NoRoots);
        }
        if backend == Backend::Openat2 && !sys::openat2_available() {
            return Err(GuardError::BackendUnavailable);
        }

        let mut resolved = Vec::with_capacity(roots.len());
        for root in roots {
            // A root that cannot be canonicalised is a configuration error, and guessing
            // is how the CVEs above happened.
            let canonical = std::fs::canonicalize(&root).map_err(|source| GuardError::Io {
                requested: root.clone(),
                source,
            })?;
            let c = sys::cstring(canonical.as_os_str()).ok_or_else(|| GuardError::MalformedPath {
                requested: canonical.clone(),
            })?;
            let dir = sys::openat(
                libc::AT_FDCWD,
                &c,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                0,
            )
            .map_err(|source| GuardError::Io {
                requested: canonical.clone(),
                source,
            })?;
            resolved.push(Root {
                display: root,
                canonical,
                dir,
            });
        }
        Ok(PathGuard {
            roots: resolved,
            backend,
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Backends usable on this machine, strongest first. Tests iterate this so the
    /// fallback is exercised on kernels that also have `openat2`.
    pub fn available_backends() -> Vec<Backend> {
        if sys::openat2_available() {
            vec![Backend::Openat2, Backend::Walk]
        } else {
            vec![Backend::Walk]
        }
    }

    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|r| r.canonical.as_path())
    }

    /// Opens an existing regular file for reading.
    pub fn open_read(&self, requested: &Path) -> Result<File, GuardError> {
        let fd = self.open_checked(requested, Intent::Read)?;
        Ok(File::from(fd))
    }

    /// Opens a regular file for writing, creating it when it does not exist and
    /// truncating it when it does.
    ///
    /// A missing leaf is the normal case, not an error case: ACP requires the client to
    /// create the file. The leaf is therefore the only component allowed not to exist,
    /// and it is created through a descriptor for its parent that was resolved under the
    /// same rules as everything else — so "does not exist yet" never becomes an excuse to
    /// skip the boundary check, which is how CVE-2025-53109 worked.
    pub fn open_write_create(&self, requested: &Path) -> Result<File, GuardError> {
        let fd = self.open_checked(requested, Intent::WriteCreate)?;
        sys::ftruncate(fd.as_raw_fd()).map_err(|source| GuardError::Io {
            requested: requested.to_path_buf(),
            source,
        })?;
        Ok(File::from(fd))
    }

    /// The absolute path this request denotes, for logs and for the UI, after proving it
    /// is inside a root.
    ///
    /// The returned path is the real one and not merely a textual join, because the
    /// resolution that validated it rejected every symlink; there is nothing left for a
    /// join to get wrong. A leaf that does not exist yet still resolves, provided its
    /// parent directory passes.
    pub fn resolve_for_display(&self, requested: &Path) -> Result<PathBuf, GuardError> {
        let (root, rel) = self.locate(requested)?;
        match self.open_at(root, &rel, requested, Intent::Probe) {
            Ok(_) => Ok(root.canonical.join(&rel)),
            Err(err) if err.is_not_found() => {
                let parent = rel.parent().unwrap_or_else(|| Path::new(""));
                self.open_at(root, parent, requested, Intent::Probe)?;
                Ok(root.canonical.join(&rel))
            }
            Err(err) => Err(err),
        }
    }

    fn open_checked(&self, requested: &Path, intent: Intent) -> Result<OwnedFd, GuardError> {
        let (root, rel) = self.locate(requested)?;
        let fd = self.open_at(root, &rel, requested, intent)?;

        // Everything from here on uses the descriptor only. The path string played its
        // last part above.
        let st = sys::fstat(fd.as_raw_fd()).map_err(|source| GuardError::Io {
            requested: requested.to_path_buf(),
            source,
        })?;
        if !sys::is_regular(&st) {
            return Err(GuardError::NotARegularFile {
                requested: requested.to_path_buf(),
            });
        }
        sys::clear_nonblock(fd.as_raw_fd()).map_err(|source| GuardError::Io {
            requested: requested.to_path_buf(),
            source,
        })?;
        Ok(fd)
    }

    /// Picks the root this request belongs to and returns the remainder, comparing path
    /// *components* rather than string prefixes.
    ///
    /// This is where CVE-2025-53110 lives: `"/x/allowed_evil/f".starts_with("/x/allowed")`
    /// is true for strings and false for components. `strip_prefix` is component-wise, so
    /// `allowed_evil` simply does not match `allowed`.
    fn locate(&self, requested: &Path) -> Result<(&Root, PathBuf), GuardError> {
        if !requested.is_absolute() {
            return Err(GuardError::NotAbsolute {
                requested: requested.to_path_buf(),
            });
        }
        for root in &self.roots {
            let rel = requested
                .strip_prefix(&root.canonical)
                .or_else(|_| requested.strip_prefix(&root.display));
            let Ok(rel) = rel else { continue };

            // `..` is refused outright instead of being folded away. Both backends could
            // in principle cope with it, but they would cope differently, and a boundary
            // check whose answer depends on which backend is active is a boundary check
            // waiting to disagree with itself. With symlinks already banned, `..` carries
            // no meaning that a caller cannot express without it.
            let mut normalised = PathBuf::new();
            for component in rel.components() {
                match component {
                    Component::Normal(part) => normalised.push(part),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        return Err(GuardError::ParentTraversal {
                            requested: requested.to_path_buf(),
                        })
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        return Err(GuardError::OutsideRoot {
                            requested: requested.to_path_buf(),
                        })
                    }
                }
            }
            return Ok((root, normalised));
        }
        Err(GuardError::OutsideRoot {
            requested: requested.to_path_buf(),
        })
    }

    fn open_at(
        &self,
        root: &Root,
        rel: &Path,
        requested: &Path,
        intent: Intent,
    ) -> Result<OwnedFd, GuardError> {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Openat2 => open_openat2(root, rel, requested, intent),
            #[cfg(not(target_os = "linux"))]
            Backend::Openat2 => Err(GuardError::BackendUnavailable),
            Backend::Walk => open_walk(root, rel, requested, intent),
        }
    }
}

#[cfg(unix)]
fn read_flags() -> libc::c_int {
    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK
}

#[cfg(unix)]
fn write_flags() -> libc::c_int {
    libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK
}

/// Probing must work for a regular file as well as a directory, so it cannot reuse the
/// directory-only walk flags.
#[cfg(target_os = "linux")]
fn probe_flags() -> libc::c_int {
    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC
}

#[cfg(all(unix, not(target_os = "linux")))]
fn probe_flags() -> libc::c_int {
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
}

/// Created files follow the process umask, like any other tool writing into the user's
/// repository would.
#[cfg(unix)]
const CREATE_MODE: u32 = 0o666;

#[cfg(unix)]
fn errno_to_guard(requested: &Path, at: &Path, source: io::Error) -> GuardError {
    match source.raw_os_error() {
        // RESOLVE_NO_SYMLINKS and O_NOFOLLOW both report a refused link this way.
        Some(libc::ELOOP) => GuardError::SymlinkEncountered {
            requested: requested.to_path_buf(),
            at: at.to_path_buf(),
        },
        // RESOLVE_BENEATH reports an attempted escape this way.
        Some(libc::EXDEV) => GuardError::OutsideRoot {
            requested: requested.to_path_buf(),
        },
        Some(libc::ENOTDIR) => GuardError::NotADirectory {
            requested: requested.to_path_buf(),
            at: at.to_path_buf(),
        },
        Some(libc::EISDIR) => GuardError::NotARegularFile {
            requested: requested.to_path_buf(),
        },
        _ => GuardError::Io {
            requested: requested.to_path_buf(),
            source,
        },
    }
}

#[cfg(target_os = "linux")]
fn open_openat2(
    root: &Root,
    rel: &Path,
    requested: &Path,
    intent: Intent,
) -> Result<OwnedFd, GuardError> {
    let rel_str: &OsStr = if rel.as_os_str().is_empty() {
        OsStr::new(".")
    } else {
        rel.as_os_str()
    };
    let c = sys::cstring(rel_str).ok_or_else(|| GuardError::MalformedPath {
        requested: requested.to_path_buf(),
    })?;

    let flags = match intent {
        Intent::Read => read_flags(),
        Intent::WriteCreate => write_flags() | libc::O_CREAT,
        Intent::Probe => libc::O_PATH | libc::O_CLOEXEC,
    };
    unsafe {
        sys::openat2(
            root.dir.as_raw_fd(),
            &c,
            flags,
            CREATE_MODE,
            sys::STRICT_RESOLVE,
        )
    }
    .map_err(|source| errno_to_guard(requested, &root.canonical.join(rel), source))
}

/// Fallback resolver: one `openat` per component from the root descriptor, with the type
/// of every intermediate result checked before it is used as the next directory.
#[cfg(unix)]
fn open_walk(
    root: &Root,
    rel: &Path,
    requested: &Path,
    intent: Intent,
) -> Result<OwnedFd, GuardError> {
    let components: Vec<&OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(part) => part,
            // `locate` has already rejected everything else.
            _ => unreachable!("locate() normalises the relative path"),
        })
        .collect();

    let mut dir = root.dir.try_clone().map_err(|source| GuardError::Io {
        requested: requested.to_path_buf(),
        source,
    })?;
    let mut walked = root.canonical.clone();

    let (last, parents) = match components.split_last() {
        Some((last, parents)) => (Some(*last), parents),
        // The request names the root itself.
        None => (None, &components[..]),
    };

    for part in parents {
        let c = sys::cstring(part).ok_or_else(|| GuardError::MalformedPath {
            requested: requested.to_path_buf(),
        })?;
        walked.push(part);
        let next = sys::openat(dir.as_raw_fd(), &c, sys::WALK_FLAGS, 0)
            .map_err(|source| errno_to_guard(requested, &walked, source))?;
        // On Linux `O_PATH | O_NOFOLLOW` happily returns a descriptor for the symlink
        // itself, so the refusal has to happen here rather than at the syscall.
        let st = sys::fstat(next.as_raw_fd()).map_err(|source| GuardError::Io {
            requested: requested.to_path_buf(),
            source,
        })?;
        if sys::is_symlink(&st) {
            return Err(GuardError::SymlinkEncountered {
                requested: requested.to_path_buf(),
                at: walked,
            });
        }
        if !sys::is_dir(&st) {
            return Err(GuardError::NotADirectory {
                requested: requested.to_path_buf(),
                at: walked,
            });
        }
        dir = next;
    }

    let Some(last) = last else {
        // Naming the root itself: hand back a descriptor for it and let the caller's type
        // check reject it for read/write intents.
        return match intent {
            Intent::Probe => Ok(dir),
            _ => Err(GuardError::NotARegularFile {
                requested: requested.to_path_buf(),
            }),
        };
    };

    let c = sys::cstring(last).ok_or_else(|| GuardError::MalformedPath {
        requested: requested.to_path_buf(),
    })?;
    walked.push(last);

    match intent {
        Intent::Read => sys::openat(dir.as_raw_fd(), &c, read_flags() | libc::O_NOFOLLOW, 0)
            .map_err(|source| errno_to_guard(requested, &walked, source)),
        Intent::Probe => sys::openat(dir.as_raw_fd(), &c, probe_flags(), 0)
            .and_then(|fd| {
                let st = sys::fstat(fd.as_raw_fd())?;
                if sys::is_symlink(&st) {
                    return Err(io::Error::from_raw_os_error(libc::ELOOP));
                }
                Ok(fd)
            })
            .map_err(|source| errno_to_guard(requested, &walked, source)),
        Intent::WriteCreate => {
            // Exclusive create first: if it succeeds, nothing existed at that name and
            // nothing could have been substituted for it. Only on EEXIST do we open what
            // is there, and then with O_NOFOLLOW so a symlink planted in the meantime is
            // refused rather than followed.
            let created = sys::openat(
                dir.as_raw_fd(),
                &c,
                write_flags() | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                CREATE_MODE,
            );
            match created {
                Ok(fd) => Ok(fd),
                Err(err) if err.raw_os_error() == Some(libc::EEXIST) => {
                    sys::openat(dir.as_raw_fd(), &c, write_flags() | libc::O_NOFOLLOW, 0)
                        .map_err(|source| errno_to_guard(requested, &walked, source))
                }
                Err(source) => Err(errno_to_guard(requested, &walked, source)),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Non-unix placeholder
// ---------------------------------------------------------------------------------------

/// Windows needs a different implementation entirely (reparse points, 8.3 short names,
/// alternate data streams, and `\\?\` prefixes each defeat a different part of the unix
/// design), so it refuses rather than pretending.
#[cfg(not(unix))]
pub struct PathGuard {
    roots: Vec<PathBuf>,
    backend: Backend,
}

#[cfg(not(unix))]
impl PathGuard {
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, GuardError> {
        Self::with_backend(roots, Backend::Walk)
    }

    pub fn with_backend(_roots: Vec<PathBuf>, _backend: Backend) -> Result<Self, GuardError> {
        Err(GuardError::UnsupportedPlatform)
    }

    pub fn available_backends() -> Vec<Backend> {
        Vec::new()
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|r| r.as_path())
    }

    pub fn open_read(&self, _requested: &Path) -> Result<File, GuardError> {
        Err(GuardError::UnsupportedPlatform)
    }

    pub fn open_write_create(&self, _requested: &Path) -> Result<File, GuardError> {
        Err(GuardError::UnsupportedPlatform)
    }

    pub fn resolve_for_display(&self, _requested: &Path) -> Result<PathBuf, GuardError> {
        Err(GuardError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_list_is_refused() {
        let err = PathGuard::new(Vec::new()).unwrap_err();
        assert_eq!(err.audit_kind(), "no-roots");
    }

    #[test]
    fn audit_kinds_separate_boundary_from_environment() {
        let outside = GuardError::OutsideRoot {
            requested: PathBuf::from("/etc/passwd"),
        };
        assert!(outside.is_boundary_violation());
        assert!(!outside.is_not_found());

        let missing = GuardError::Io {
            requested: PathBuf::from("/tmp/nope"),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(missing.is_not_found());
        assert!(!missing.is_boundary_violation());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_how_layout_matches_the_kernel_abi() {
        // The kernel identifies the struct version by its size; a mismatch here would be
        // reported as E2BIG at runtime, far away from the cause.
        assert_eq!(std::mem::size_of::<sys::OpenHow>(), 24);
        assert_eq!(sys::STRICT_RESOLVE, 0x02 | 0x04 | 0x08);
    }
}
