//! Real paths for the Seatbelt profile.
//!
//! Seatbelt matches the path the kernel resolved, with every symbolic link followed and in the
//! spelling stored on disk (APFS is usually case-insensitive and normalization-insensitive, but
//! `literal` and `subpath` compare bytes). A grant spelled `/tmp/x` or `/Users/Me` would never
//! match `/private/tmp/x` or `/Users/me`, so every path goes through [`resolve`] first.
//!
//! [`resolve`] is one walk that is the only source of its result: it opens each component
//! relative to the previous one without following links (`openat` with `O_NOFOLLOW`), reads
//! links itself, and takes each spelling from the opened file (`F_GETPATH`). Nothing is resolved
//! twice, so a link that changes during the walk can't make a check and the result disagree. Below
//! a "stop" (a writable root, where the plugin can create links) it follows nothing.

use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// How many links one walk follows before giving up, like the kernel's `MAXSYMLINKS`. A loop
/// never resolves, so whatever it guards fails closed.
const MAX_LINKS: usize = 32;

/// `openat` flags: no following a final link (it fails with `ELOOP` instead), `O_EVTONLY` so
/// that opening needs no read permission, and `O_NONBLOCK` and `O_NOCTTY` so that a FIFO or a
/// terminal at a walked path can't block the trampoline or become its controlling terminal.
const OPEN_FLAGS: libc::c_int =
    libc::O_EVTONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY;

/// What resolving one path found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resolved {
    /// The real path: every link before a stop followed, existing components spelled as
    /// stored, and the rest (missing components, or everything after a stop) appended as
    /// spelled.
    pub real: PathBuf,
    /// Where the links the walk followed are, each as its name inside the real path of its
    /// directory.
    pub links: Vec<PathBuf>,
    /// Everything the walk looked up (directories, links, and the final component), which the
    /// process needs `file-read-metadata` on to get there. Excludes `/`.
    pub visited: Vec<PathBuf>,
}

/// Resolves the absolute `path`, following no link at or below any of `stops` (real paths).
///
/// Once the walk reaches a stop, the rest of the path is appended as spelled: a link planted
/// there later leads somewhere the profile doesn't grant, and a wrongly cased spelling never
/// matches, so both fail closed. A missing component (a grant that doesn't exist yet, or a
/// dangling link's target) and everything after it are appended as spelled, and so is the rest
/// of the path at a link loop.
pub(super) fn resolve(path: &Path, stops: &[PathBuf]) -> Resolved {
    let mut pending = names(path);
    let mut links = Vec::new();
    let mut visited = Vec::new();
    let mut cur = PathBuf::from("/");
    if let Ok(mut dir) = open_root() {
        while let Some(name) = pending.pop_front() {
            if stops.iter().any(|stop| cur.starts_with(stop)) {
                pending.push_front(name);
                break;
            }
            match open_at(dir.as_fd(), &name) {
                Ok(file) => {
                    let real = stored_path(&file, &cur).unwrap_or_else(|| cur.join(&name));
                    visited.push(real.clone());
                    cur = real;
                    dir = file;
                }
                Err(err) if err.raw_os_error() == Some(libc::ELOOP) && links.len() < MAX_LINKS => {
                    let Ok(target) = read_link_at(dir.as_fd(), &name) else {
                        pending.push_front(name);
                        break;
                    };
                    let location = cur.join(&name);
                    visited.push(location.clone());
                    links.push(location);
                    if target.is_absolute() {
                        let Ok(root) = open_root() else {
                            pending.push_front(name);
                            break;
                        };
                        dir = root;
                        cur = PathBuf::from("/");
                    }
                    for name in names(&target).into_iter().rev() {
                        pending.push_front(name);
                    }
                }
                Err(_) => {
                    pending.push_front(name);
                    break;
                }
            }
        }
    }
    let real = pending.into_iter().fold(cur, |acc, name| acc.join(name));
    Resolved {
        real,
        links,
        visited,
    }
}

/// The components to walk: names and `..`.
fn names(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Where `file`, just opened by name in the directory `dir`, is, as stored. A directory's path
/// is unambiguous; for anything else only its name is taken, and only when the path is in
/// `dir`, because a file with several hard links may report another one.
fn stored_path(file: &File, dir: &Path) -> Option<PathBuf> {
    let path = fd_path(file.as_fd()).ok()?;
    if file.metadata().ok()?.is_dir() || path.parent() == Some(dir) {
        Some(path)
    } else {
        None
    }
}

fn open_root() -> io::Result<File> {
    let root = c"/";
    // SAFETY: `root` is a valid NUL-terminated string; on success the new descriptor is ours.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::open(root.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    owned(fd)
}

fn open_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `name` a valid NUL-terminated string that outlive
    // the call; on success the new descriptor is ours.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), OPEN_FLAGS) };
    owned(fd)
}

/// Takes ownership of a descriptor a syscall returned, or its error.
fn owned(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor that nothing else owns.
    #[allow(unsafe_code)]
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(File::from(fd))
}

/// The path of an open file (`F_GETPATH`).
fn fd_path(fd: BorrowedFd<'_>) -> io::Result<PathBuf> {
    let mut buf = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` writes at most `PATH_MAX` bytes, including the NUL, into `buf`.
    #[allow(unsafe_code)]
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// The target of the link `name` in `dir`.
fn read_link_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<PathBuf> {
    let name = CString::new(name.as_bytes())?;
    let mut buf = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `dir` is an open descriptor, `name` a valid NUL-terminated string, and `readlinkat`
    // writes at most `buf.len()` bytes into `buf`.
    #[allow(unsafe_code)]
    let len = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let len = usize::try_from(len).map_err(|_| io::Error::last_os_error())?;
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn real(path: &Path) -> PathBuf {
        resolve(path, &[]).real
    }

    #[test]
    fn links_are_followed_and_missing_tails_kept() {
        assert_eq!(real(Path::new("/tmp")), Path::new("/private/tmp"));
        assert_eq!(
            real(Path::new("/tmp/./missing/deeper/")),
            Path::new("/private/tmp/missing/deeper")
        );
        assert_eq!(real(Path::new("/")), Path::new("/"));

        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(real_dir.join("a/real")).unwrap();
        fs::create_dir(real_dir.join("b")).unwrap();
        // `first` -> `b/second` (relative) -> `a/real` (absolute).
        symlink(real_dir.join("a/real"), real_dir.join("b/second")).unwrap();
        symlink("b/second", real_dir.join("first")).unwrap();
        let resolved = resolve(&dir.path().join("first/file"), &[]);
        assert_eq!(resolved.real, real_dir.join("a/real/file"));
        // `/var` (the temporary directory is under it), then the two links.
        assert_eq!(
            resolved.links,
            [
                PathBuf::from("/var"),
                real_dir.join("first"),
                real_dir.join("b/second")
            ]
        );
        assert_eq!(real(&real_dir.join("b/../a/real")), real_dir.join("a/real"));

        // A dangling link leads to its target's place.
        symlink(real_dir.join("gone"), real_dir.join("dangling")).unwrap();
        assert_eq!(
            real(&real_dir.join("dangling/x")),
            real_dir.join("gone/x"),
            "the target's missing tail is kept"
        );
        // A loop ends, unresolved.
        symlink(real_dir.join("loop2"), real_dir.join("loop1")).unwrap();
        symlink(real_dir.join("loop1"), real_dir.join("loop2")).unwrap();
        let looped = resolve(&real_dir.join("loop1/x"), &[]);
        assert_eq!(looped.links.len(), MAX_LINKS);
        assert!(looped.real.ends_with("x"), "{looped:?}");
    }

    /// APFS volumes are case-insensitive by default; the profile must use the stored spelling.
    #[test]
    fn existing_components_get_the_stored_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(real_dir.join("Data")).unwrap();
        fs::write(real_dir.join("Data/File"), "").unwrap();
        if fs::metadata(real_dir.join("DATA")).is_err() {
            return; // A case-sensitive volume: nothing to normalize.
        }
        assert_eq!(real(&real_dir.join("DATA")), real_dir.join("Data"));
        assert_eq!(
            real(&real_dir.join("data/file")),
            real_dir.join("Data/File")
        );
    }

    /// Below a stop nothing is followed or respelled: a link planted there resolves to its own
    /// location, which the profile grants nothing through.
    #[test]
    fn nothing_is_followed_below_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().join("data");
        let victim = fs::canonicalize(dir.path()).unwrap().join("victim");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&victim).unwrap();
        fs::create_dir(root.join("Sub")).unwrap();
        symlink(&victim, root.join("notes")).unwrap();
        let stops = [root.clone()];

        let resolved = resolve(&dir.path().join("data/notes/secret"), &stops);
        assert_eq!(resolved.real, root.join("notes/secret"));
        assert!(
            !resolved.links.contains(&root.join("notes")),
            "{resolved:?}"
        );
        // The way to the stop is resolved as usual.
        assert_eq!(resolve(&dir.path().join("data"), &stops).real, root);
        if fs::metadata(root.join("SUB")).is_ok() {
            assert_eq!(resolve(&root.join("SUB"), &stops).real, root.join("SUB"));
        }
        // Without the stop the link is followed.
        assert_eq!(real(&root.join("notes/secret")), victim.join("secret"));
    }

    /// Opening a FIFO without `O_NONBLOCK` would wait for a writer forever.
    #[test]
    fn a_fifo_on_the_way_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(real_dir.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(real(&dir.path().join("fifo")), real_dir.join("fifo"));
    }

    #[test]
    fn visited_covers_links_and_ancestors() {
        let visited = resolve(Path::new("/tmp/x"), &[]).visited;
        for expected in ["/tmp", "/private", "/private/tmp"] {
            assert!(visited.contains(&PathBuf::from(expected)), "{expected}");
        }
        assert!(!visited.contains(&PathBuf::from("/")));

        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        let visited = resolve(&dir.path().join("f"), &[]).visited;
        for ancestor in real_dir.ancestors().filter(|p| p.parent().is_some()) {
            assert!(visited.contains(&ancestor.to_owned()), "{ancestor:?}");
        }
    }
}
