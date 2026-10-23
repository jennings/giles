//! Opening grants for Landlock rules.
//!
//! A Landlock rule is attached to the inode behind a file descriptor, so a grant is only as good
//! as the descriptor it's built from. [`resolve`] opens each grant in one walk that is the only
//! source of the rule: it opens each component relative to the previous one with
//! `openat(O_PATH | O_NOFOLLOW)`, reads each link it meets itself and continues with the target,
//! and hands back the descriptor of the last component. Nothing is looked up twice, so a link that
//! changes during the walk can't make a check and the rule disagree. Below a "stop" (a writable
//! root, where the command can create links) it follows nothing: a link there ends the walk
//! without a descriptor, and the grant gives nothing.

use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::sandbox::roots::{MAX_LINKS, Walk, names};

/// `openat` flags: a path-only descriptor (no read permission needed, and a FIFO or device on the
/// way isn't opened, so it can't block), never following a final link (the descriptor is then the
/// link itself).
const OPEN_FLAGS: libc::c_int = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// What resolving one path found.
#[derive(Debug)]
pub(super) struct Resolved {
    /// The real path: every link before a stop followed, and the rest (missing components, or
    /// everything after a link below a stop) appended as spelled. For logs and for comparing
    /// with the stops; the rule comes from [`Resolved::target`].
    pub real: PathBuf,
    /// Where the links the walk followed are, as real paths.
    pub links: Vec<PathBuf>,
    /// What the walk ended at.
    pub target: Target,
}

/// Where a walk ended.
#[derive(Debug)]
pub(super) enum Target {
    /// The path exists: its `O_PATH` descriptor, to build the rule from.
    Found {
        /// The descriptor.
        fd: OwnedFd,
        /// Whether it's a directory (a rule on anything else may only carry file rights).
        dir: bool,
    },
    /// A link at or below a stop, which the walk doesn't follow: the command may have planted it.
    LinkInRoot(PathBuf),
    /// A component can't be opened (usually missing), or there were too many links.
    Unreachable(io::Error),
}

impl Walk for Resolved {
    fn real(&self) -> &Path {
        &self.real
    }

    fn links(&self) -> &[PathBuf] {
        &self.links
    }
}

/// Opens the absolute `path`, following no link at or below any of `stops` (real paths).
pub(super) fn resolve(path: &Path, stops: &[PathBuf]) -> Resolved {
    let mut pending = names(path);
    let mut links = Vec::new();
    let mut cur = PathBuf::from("/");
    let target = match walk(&mut pending, &mut links, &mut cur, stops) {
        Ok((fd, dir)) => Target::Found { fd, dir },
        Err(end) => end,
    };
    let real = pending.into_iter().fold(cur, |acc, name| acc.join(name));
    Resolved {
        real,
        links,
        target,
    }
}

/// The walk itself. On success `pending` is empty; otherwise it holds the component that ended
/// the walk and everything after it.
fn walk(
    pending: &mut VecDeque<OsString>,
    links: &mut Vec<PathBuf>,
    cur: &mut PathBuf,
    stops: &[PathBuf],
) -> Result<(OwnedFd, bool), Target> {
    let mut dir = open_root().map_err(Target::Unreachable)?;
    let mut is_dir = true;
    while let Some(name) = pending.pop_front() {
        let below_stop = stops.iter().any(|stop| cur.starts_with(stop));
        let opened = open_at(&dir, &name).and_then(|fd| Ok((file_type(&fd)?, fd)));
        let (kind, fd) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                pending.push_front(name);
                return Err(Target::Unreachable(err));
            }
        };
        if kind == libc::S_IFLNK {
            let location = cur.join(&name);
            if below_stop {
                pending.push_front(name);
                return Err(Target::LinkInRoot(location));
            }
            if links.len() >= MAX_LINKS {
                pending.push_front(name);
                return Err(Target::Unreachable(io::Error::from_raw_os_error(
                    libc::ELOOP,
                )));
            }
            let target = match read_link(&fd) {
                Ok(target) => target,
                Err(err) => {
                    pending.push_front(name);
                    return Err(Target::Unreachable(err));
                }
            };
            links.push(location);
            if target.is_absolute() {
                dir = open_root().map_err(Target::Unreachable)?;
                *cur = PathBuf::from("/");
                is_dir = true;
            }
            for name in names(&target).into_iter().rev() {
                pending.push_front(name);
            }
            continue;
        }
        if name == ".." {
            cur.pop();
        } else {
            cur.push(&name);
        }
        dir = fd;
        is_dir = kind == libc::S_IFDIR;
    }
    Ok((dir, is_dir))
}

fn open_root() -> io::Result<OwnedFd> {
    let root = c"/";
    // SAFETY: `root` is a valid NUL-terminated string; on success the new descriptor is ours.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::open(root.as_ptr(), OPEN_FLAGS | libc::O_DIRECTORY) };
    owned(fd)
}

fn open_at(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `name` a valid NUL-terminated string that outlive
    // the call; on success the new descriptor is ours.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), OPEN_FLAGS) };
    owned(fd)
}

/// Takes ownership of a descriptor a syscall returned, or its error.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor that nothing else owns.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The file type bits (`S_IFMT`) of an open file.
fn file_type(fd: &OwnedFd) -> io::Result<libc::mode_t> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is open and `stat` has room for a `struct stat`, which `fstat` fills on
    // success.
    #[allow(unsafe_code)]
    let stat = unsafe {
        if libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        stat.assume_init()
    };
    Ok(stat.st_mode & libc::S_IFMT)
}

/// The target of the link `link`, opened with `O_PATH | O_NOFOLLOW` (`readlinkat` with an empty
/// path reads the link the descriptor refers to).
fn read_link(link: &OwnedFd) -> io::Result<PathBuf> {
    let mut buf = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `link` is an open descriptor, the path is a valid empty NUL-terminated string, and
    // `readlinkat` writes at most `buf.len()` bytes into `buf`.
    #[allow(unsafe_code)]
    let len = unsafe {
        libc::readlinkat(
            link.as_raw_fd(),
            c"".as_ptr(),
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
    use std::os::unix::fs::{MetadataExt, symlink};

    use super::*;

    /// The inode number behind a walk's descriptor.
    fn inode(resolved: &Resolved) -> u64 {
        let Target::Found { fd, .. } = &resolved.target else {
            panic!("not found: {resolved:?}");
        };
        let file = fs::File::from(fd.try_clone().unwrap());
        file.metadata().unwrap().ino()
    }

    fn ino(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().ino()
    }

    #[test]
    fn links_are_followed_to_the_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(real_dir.join("a/real")).unwrap();
        fs::write(real_dir.join("a/real/file"), "x").unwrap();
        fs::create_dir(real_dir.join("b")).unwrap();
        // `first` -> `b/second` (relative) -> `a/real` (absolute).
        symlink(real_dir.join("a/real"), real_dir.join("b/second")).unwrap();
        symlink("b/second", real_dir.join("first")).unwrap();

        let resolved = resolve(&real_dir.join("first/file"), &[]);
        assert_eq!(resolved.real, real_dir.join("a/real/file"));
        assert_eq!(
            resolved.links,
            [real_dir.join("first"), real_dir.join("b/second")]
        );
        assert_eq!(inode(&resolved), ino(&real_dir.join("a/real/file")));
        assert!(matches!(resolved.target, Target::Found { dir: false, .. }));

        let parent = resolve(&real_dir.join("b/../a/real"), &[]);
        assert_eq!(parent.real, real_dir.join("a/real"));
        assert!(matches!(parent.target, Target::Found { dir: true, .. }));
        assert_eq!(inode(&parent), ino(&real_dir.join("a/real")));

        let root = resolve(Path::new("/"), &[]);
        assert_eq!(root.real, Path::new("/"));
        assert_eq!(inode(&root), ino(Path::new("/")));
    }

    #[test]
    fn missing_paths_and_loops_end_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        let missing = resolve(&real_dir.join("missing/deeper"), &[]);
        assert_eq!(missing.real, real_dir.join("missing/deeper"));
        assert!(matches!(&missing.target, Target::Unreachable(err)
            if err.kind() == io::ErrorKind::NotFound));

        // A dangling link leads to its target's place.
        symlink(real_dir.join("gone"), real_dir.join("dangling")).unwrap();
        let dangling = resolve(&real_dir.join("dangling/x"), &[]);
        assert_eq!(dangling.real, real_dir.join("gone/x"));
        assert!(matches!(dangling.target, Target::Unreachable(_)));

        symlink(real_dir.join("loop2"), real_dir.join("loop1")).unwrap();
        symlink(real_dir.join("loop1"), real_dir.join("loop2")).unwrap();
        let looped = resolve(&real_dir.join("loop1/x"), &[]);
        assert_eq!(looped.links.len(), MAX_LINKS);
        assert!(matches!(&looped.target, Target::Unreachable(err)
            if err.raw_os_error() == Some(libc::ELOOP)));
    }

    /// Below a stop nothing is followed: a link planted there gives no descriptor at all, so the
    /// grant gives nothing.
    #[test]
    fn nothing_is_followed_below_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = fs::canonicalize(dir.path()).unwrap();
        let root = real_dir.join("data");
        let victim = real_dir.join("victim");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir(&victim).unwrap();
        symlink(&victim, root.join("notes")).unwrap();
        let stops = [root.clone()];

        let resolved = resolve(&root.join("notes/secret"), &stops);
        assert_eq!(resolved.real, root.join("notes/secret"));
        assert!(resolved.links.is_empty(), "{resolved:?}");
        assert!(
            matches!(&resolved.target, Target::LinkInRoot(link) if *link == root.join("notes"))
        );
        // The final component too.
        assert!(matches!(
            resolve(&root.join("notes"), &stops).target,
            Target::LinkInRoot(_)
        ));
        // Real directories below the stop, and the way to it, resolve as usual.
        let sub = resolve(&root.join("sub"), &stops);
        assert_eq!(inode(&sub), ino(&root.join("sub")));
        assert_eq!(inode(&resolve(&root, &stops)), ino(&root));
        // Without the stop the link is followed.
        assert_eq!(
            resolve(&root.join("notes"), &[]).real,
            victim,
            "the link is followed without a stop"
        );
    }

    /// `O_PATH` doesn't open the file, so a FIFO on the way can't block the walk.
    #[test]
    fn a_fifo_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated string.
        #[allow(unsafe_code)]
        let ret = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(ret, 0);
        let resolved = resolve(&fifo, &[]);
        assert!(matches!(resolved.target, Target::Found { dir: false, .. }));
    }
}
