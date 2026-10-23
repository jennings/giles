//! A skill's resource files (PLAN section 6.6): the files next to `SKILL.md` that its body may
//! refer to, listed for the model and read for `load_skill` so the core can hand them over like
//! any other media.
//!
//! Only what the listing could show is readable: regular files under the skill's directory, at
//! most [`MAX_RESOURCE_DEPTH`] directories down, with no hidden component (a name starting with
//! `.`). A requested path is resolved with every symbolic link followed and must still be inside
//! the skill's (canonical) directory, so neither `..` nor a link can reach other files. Skill
//! directories are the operator's, so a link inside one that points elsewhere in it is fine.
//!
//! Blocking file I/O: async callers run it on a blocking thread.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::SKILL_FILE;

/// The most files a listing shows.
pub const MAX_RESOURCE_FILES: usize = 200;

/// How many directories down a resource file may be (`a/b/c.txt` is two).
pub const MAX_RESOURCE_DEPTH: usize = 8;

/// The largest resource file that can be read.
pub const MAX_RESOURCE_BYTES: u64 = 10 * 1024 * 1024;

/// A skill's resource files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceList {
    /// Paths relative to the skill's directory, `/`-separated, sorted.
    pub files: Vec<String>,
    /// Whether there were more than [`MAX_RESOURCE_FILES`]; the first ones are listed.
    pub truncated: bool,
}

/// Why a resource file can't be read. The texts name the path as requested, never where the
/// skill lives.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResourceError {
    /// The path isn't a readable file of the skill: malformed, hidden, too deep, missing, not a
    /// regular file, or resolving outside the skill's directory.
    #[error("the skill has no file `{0}`")]
    NotFound(String),
    /// The file is larger than [`MAX_RESOURCE_BYTES`].
    #[error("`{path}` is {size} bytes, more than the {max} a skill file may have")]
    TooLarge {
        /// The path as requested.
        path: String,
        /// The file's size.
        size: u64,
        /// [`MAX_RESOURCE_BYTES`].
        max: u64,
    },
    /// The skill directory or the file couldn't be read.
    #[error("cannot read `{path}`: {error}")]
    Io {
        /// The path as requested (empty for the skill's directory).
        path: String,
        /// The underlying error.
        error: io::Error,
    },
}

/// One resource file, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Its path relative to the skill's directory, `/`-separated, as the listing shows it.
    pub path: String,
    /// Its contents.
    pub bytes: Vec<u8>,
}

impl Resource {
    /// The file's name: the last component of its path.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// The resource files of the skill in `dir`: every regular file below it except its own
/// `SKILL.md`, skipping hidden names, non-UTF-8 names, and anything deeper than
/// [`MAX_RESOURCE_DEPTH`]. A symbolic link is listed when it resolves to a regular file inside the
/// skill's directory; links to directories aren't followed.
///
/// # Errors
///
/// The skill's directory can't be resolved or read. Unreadable subdirectories are skipped.
pub fn list_resources(dir: &Path) -> Result<ResourceList, ResourceError> {
    let root = fs::canonicalize(dir).map_err(|error| ResourceError::Io {
        path: String::new(),
        error,
    })?;
    let mut list = ResourceList::default();
    walk(&root, &root, "", 0, &mut list).map_err(|error| ResourceError::Io {
        path: String::new(),
        error,
    })?;
    list.files.sort();
    Ok(list)
}

fn walk(
    root: &Path,
    dir: &Path,
    prefix: &str,
    depth: usize,
    list: &mut ResourceList,
) -> io::Result<()> {
    let mut entries: Vec<(String, fs::DirEntry)> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry)))
        .filter(|(name, _)| !is_hidden(name))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, entry) in entries {
        if list.files.len() > MAX_RESOURCE_FILES {
            break;
        }
        let relative = format!("{prefix}{name}");
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_RESOURCE_DEPTH {
                // A subdirectory that can't be read is left out.
                let _ = walk(
                    root,
                    &entry.path(),
                    &format!("{relative}/"),
                    depth + 1,
                    list,
                );
            }
            continue;
        }
        if depth == 0 && name == SKILL_FILE {
            continue;
        }
        let listed = kind.is_file()
            || (kind.is_symlink()
                && fs::canonicalize(entry.path())
                    .is_ok_and(|target| target.starts_with(root) && target.is_file()));
        if listed {
            list.files.push(relative);
        }
    }
    if list.files.len() > MAX_RESOURCE_FILES {
        list.files.truncate(MAX_RESOURCE_FILES);
        list.truncated = true;
    }
    Ok(())
}

/// The path `requested` names inside a skill, split into its components, or `None` if it can't
/// name a listed file: empty, absolute, with an empty, `.`, `..`, or hidden component, or deeper
/// than [`MAX_RESOURCE_DEPTH`].
fn components(requested: &str) -> Option<Vec<&str>> {
    if requested.is_empty() || requested.starts_with('/') || requested.contains('\0') {
        return None;
    }
    let parts: Vec<&str> = requested.split('/').collect();
    let bad = |part: &&str| part.is_empty() || *part == ".." || is_hidden(part);
    if parts.iter().any(bad) || parts.len() > MAX_RESOURCE_DEPTH + 1 {
        return None;
    }
    Some(parts)
}

/// Reads resource file `requested` (a path relative to the skill's directory, as
/// [`list_resources`] shows it) of the skill in `dir`. The path is resolved with symbolic links
/// followed and must name a regular file inside the skill's canonical directory.
///
/// # Errors
///
/// [`ResourceError::NotFound`] for a path that isn't such a file (including every way of
/// pointing outside the skill), [`ResourceError::TooLarge`], or [`ResourceError::Io`].
pub fn read_resource(dir: &Path, requested: &str) -> Result<Resource, ResourceError> {
    let not_found = || ResourceError::NotFound(requested.to_owned());
    let parts = components(requested).ok_or_else(not_found)?;
    let root = fs::canonicalize(dir).map_err(|error| ResourceError::Io {
        path: String::new(),
        error,
    })?;
    let path: PathBuf = parts
        .iter()
        .fold(root.clone(), |path, part| path.join(part));
    let target = match fs::canonicalize(&path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(not_found()),
        Err(error) => {
            return Err(ResourceError::Io {
                path: requested.to_owned(),
                error,
            });
        }
    };
    if !target.starts_with(&root) || target == root {
        return Err(not_found());
    }
    let io_error = |error| ResourceError::Io {
        path: requested.to_owned(),
        error,
    };
    // Not a regular file (a FIFO would block the open): refused before opening.
    if !fs::metadata(&target).is_ok_and(|meta| meta.is_file()) {
        return Err(not_found());
    }
    // The canonical path has no link in it, so a link (or FIFO) swapped in since is refused or
    // can't block: `O_NOFOLLOW` for the last component, `O_NONBLOCK` for the open.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&target)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => not_found(),
            _ => io_error(error),
        })?;
    // A directory above it swapped for a link would still be followed: the file actually opened
    // must be inside the skill.
    let opened = fd_path(&file).map_err(io_error)?;
    if !opened.starts_with(&root) || opened == root {
        return Err(not_found());
    }
    let meta = file.metadata().map_err(io_error)?;
    if !meta.is_file() {
        return Err(not_found());
    }
    let too_large = |size| ResourceError::TooLarge {
        path: requested.to_owned(),
        size,
        max: MAX_RESOURCE_BYTES,
    };
    if meta.len() > MAX_RESOURCE_BYTES {
        return Err(too_large(meta.len()));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or_default());
    // One byte past the limit shows a file that grew meanwhile.
    file.take(MAX_RESOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_RESOURCE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(Resource {
        path: parts.join("/"),
        bytes,
    })
}

/// The path of the file `file` has open, as the kernel sees it now.
#[cfg(target_os = "linux")]
fn fd_path(file: &fs::File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// The path of the file `file` has open, as the kernel sees it now (`F_GETPATH`).
#[cfg(target_os = "macos")]
fn fd_path(file: &fs::File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `file` is an open descriptor, and `F_GETPATH` writes at most `PATH_MAX` bytes,
    // including the NUL, into `buf`.
    #[allow(unsafe_code)]
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A skill `demo` in a temporary directory, with resources, hidden files, and links in and
    /// out; next to it, a secret file.
    fn skill() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills/demo");
        let write = |relative: &str, contents: &str| {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        };
        write(
            SKILL_FILE,
            "---\nname: demo\ndescription: Demo.\n---\nBody.\n",
        );
        write("reference.md", "# Reference");
        write("scripts/run.py", "print('hi')");
        write("scripts/deep/data.csv", "a,b");
        write(".env", "TOKEN=x");
        write(".git/config", "[core]");
        write("scripts/.cache/x", "x");
        fs::write(tmp.path().join("secret.txt"), "the secret").unwrap();
        symlink(dir.join("reference.md"), dir.join("alias.md")).unwrap();
        symlink(tmp.path().join("secret.txt"), dir.join("escape.txt")).unwrap();
        symlink(tmp.path(), dir.join("outside")).unwrap();
        symlink(dir.join("missing"), dir.join("dangling")).unwrap();
        (tmp, dir)
    }

    #[test]
    fn lists_visible_regular_files_and_links_that_stay_inside() {
        let (_tmp, dir) = skill();
        let list = list_resources(&dir).unwrap();
        assert_eq!(
            list.files,
            [
                "alias.md",
                "reference.md",
                "scripts/deep/data.csv",
                "scripts/run.py"
            ]
        );
        assert!(!list.truncated);
    }

    #[test]
    fn listings_stop_at_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..=MAX_RESOURCE_FILES {
            fs::write(tmp.path().join(format!("f{n:04}.txt")), "x").unwrap();
        }
        let list = list_resources(tmp.path()).unwrap();
        assert_eq!(list.files.len(), MAX_RESOURCE_FILES);
        assert!(list.truncated);
        assert_eq!(list.files[0], "f0000.txt");
    }

    #[test]
    fn reads_listed_files() {
        let (_tmp, dir) = skill();
        let resource = read_resource(&dir, "scripts/run.py").unwrap();
        assert_eq!(resource.path, "scripts/run.py");
        assert_eq!(resource.name(), "run.py");
        assert_eq!(resource.bytes, b"print('hi')");
        // A link inside the skill reads its target.
        assert_eq!(
            read_resource(&dir, "alias.md").unwrap().bytes,
            b"# Reference"
        );
        // The skill file itself is in the skill.
        assert!(read_resource(&dir, SKILL_FILE).is_ok());
    }

    #[test]
    fn nothing_outside_the_skill_or_hidden_can_be_read() {
        let (tmp, dir) = skill();
        let secret = tmp.path().join("secret.txt");
        for requested in [
            "",
            "/etc/passwd",
            secret.to_str().unwrap(),
            "../../secret.txt",
            "scripts/../../../secret.txt",
            "scripts/../reference.md",
            "./reference.md",
            "scripts//run.py",
            "escape.txt",
            "outside/secret.txt",
            "dangling",
            ".env",
            ".git/config",
            "scripts/.cache/x",
            "scripts",
            "missing.txt",
            "a\0b",
        ] {
            match read_resource(&dir, requested) {
                Err(ResourceError::NotFound(path)) => assert_eq!(path, requested),
                other => panic!("{requested:?}: {other:?}"),
            }
        }
        let too_deep = ["d"; MAX_RESOURCE_DEPTH + 1].join("/") + "/f.txt";
        assert!(matches!(
            read_resource(&dir, &too_deep),
            Err(ResourceError::NotFound(_))
        ));
    }

    #[test]
    fn fifos_are_refused_without_blocking() {
        let (_tmp, dir) = skill();
        let fifo = dir.join("pipe");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(made, 0, "{}", io::Error::last_os_error());
        assert!(
            !list_resources(&dir)
                .unwrap()
                .files
                .contains(&"pipe".to_owned())
        );
        assert!(matches!(
            read_resource(&dir, "pipe"),
            Err(ResourceError::NotFound(_))
        ));
    }

    #[test]
    fn large_files_are_refused() {
        let (_tmp, dir) = skill();
        let file = fs::File::create(dir.join("big.bin")).unwrap();
        file.set_len(MAX_RESOURCE_BYTES + 1).unwrap();
        assert!(matches!(
            read_resource(&dir, "big.bin"),
            Err(ResourceError::TooLarge { size, .. }) if size == MAX_RESOURCE_BYTES + 1
        ));
    }

    #[test]
    fn errors_never_show_where_the_skill_lives() {
        let (tmp, dir) = skill();
        let text = read_resource(&dir, "../secret.txt")
            .unwrap_err()
            .to_string();
        assert_eq!(text, "the skill has no file `../secret.txt`");
        assert!(!text.contains(tmp.path().to_str().unwrap()));
    }
}
