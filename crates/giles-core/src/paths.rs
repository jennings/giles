//! The data root (`GILES_HOME`): where everything lives, and creating its layout (PLAN section
//! 11.1).
//!
//! [`DataRoot`] is the one place that knows the layout. Every other part of Giles asks it for paths
//! instead of joining strings onto the root. Constructing a `DataRoot` does no I/O, so a CLI client
//! can compute the same socket paths as the daemon; [`DataRoot::create_layout`] creates the
//! directories.
//!
//! Unix socket paths are short (`sun_path` is 104 bytes on macOS and 108 on Linux, including the
//! terminating NUL). When the admin or proxy socket under `<root>/run/` would be too long, both
//! sockets move to a private directory under `$TMPDIR` named after a hash of the root, so the daemon
//! and its CLI clients agree on it without talking to each other.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The environment variable that selects the data root.
pub const GILES_HOME_ENV: &str = "GILES_HOME";

/// The data root's directory name under `$HOME` when `GILES_HOME` is not set.
pub const DEFAULT_DIR_NAME: &str = ".giles";

/// Size of `sockaddr_un.sun_path`, including the terminating NUL.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SUN_PATH_LEN: usize = 108;
/// Size of `sockaddr_un.sun_path`, including the terminating NUL (macOS and the BSDs).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SUN_PATH_LEN: usize = 104;

/// The longest Unix socket path, in bytes, that this platform can bind: 103 on macOS, 107 on
/// Linux.
pub const MAX_SOCKET_PATH_LEN: usize = SUN_PATH_LEN - 1;

/// Mode of every directory Giles creates in the data root, and of `run/` and the socket directory
/// even when they already exist.
const PRIVATE_DIR_MODE: u32 = 0o700;

const ADMIN_SOCKET: &str = "admin.sock";
const PROXY_SOCKET: &str = "proxy.sock";
/// Every socket that lives in the socket directory; all of them must fit in `sun_path`.
const SOCKET_NAMES: [&str; 2] = [ADMIN_SOCKET, PROXY_SOCKET];

/// Errors from resolving or creating the data root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PathsError {
    /// Neither an explicit root, `GILES_HOME`, nor `HOME` is set.
    #[error("cannot locate the data root: neither GILES_HOME nor HOME is set")]
    NoHome,
    /// The data root was given as an empty path.
    #[error("the data root path is empty")]
    EmptyRoot,
    /// A path is relative and the current directory is unavailable.
    #[error("cannot make {} absolute", .path.display())]
    Absolute {
        /// The relative path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Neither `<root>/run/` nor the `$TMPDIR` fallback gives a socket path short enough to bind.
    #[error(
        "socket path {} is longer than {MAX_SOCKET_PATH_LEN} bytes, and so is the fallback {}; \
         choose a shorter GILES_HOME or TMPDIR",
        .run_socket.display(),
        .fallback_socket.display()
    )]
    SocketPathTooLong {
        /// The socket path under `<root>/run/`.
        run_socket: PathBuf,
        /// The socket path under the `$TMPDIR` fallback directory.
        fallback_socket: PathBuf,
    },
    /// The `$TMPDIR` fallback socket directory is not a private directory of ours.
    #[error("refusing to use socket directory {}: {reason}", .path.display())]
    UnsafeSocketDir {
        /// The fallback socket directory.
        path: PathBuf,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The sockets need the `$TMPDIR` fallback, but the temporary directory is empty.
    #[error(
        "the data root is too long for Unix socket paths, and the temporary directory is empty"
    )]
    EmptyTempDir,
    /// An agent, user, or instance name is not a single path component.
    #[error("invalid name {name:?}: it must be a single path component")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
    /// A filesystem operation failed.
    #[error("cannot {action} {}", .path.display())]
    Io {
        /// What was being done, e.g. "create directory".
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// Chooses the data root path: `explicit` (the `--home` flag) if given, else `giles_home` (the
/// `GILES_HOME` variable) if set and non-empty, else `$HOME/.giles`.
///
/// The result may be relative; [`DataRoot::new`] makes it absolute.
///
/// # Errors
///
/// [`PathsError::EmptyRoot`] if `explicit` is empty, [`PathsError::NoHome`] if no source gives a
/// path.
pub fn resolve_root(
    explicit: Option<&Path>,
    giles_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, PathsError> {
    if let Some(path) = explicit {
        if path.as_os_str().is_empty() {
            return Err(PathsError::EmptyRoot);
        }
        return Ok(path.to_path_buf());
    }
    if let Some(path) = giles_home.filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    match home.filter(|p| !p.is_empty()) {
        Some(home) => Ok(Path::new(home).join(DEFAULT_DIR_NAME)),
        None => Err(PathsError::NoHome),
    }
}

/// Whether `path` is short enough to bind as a Unix socket on this platform.
pub fn socket_path_fits(path: &Path) -> bool {
    path.as_os_str().len() <= MAX_SOCKET_PATH_LEN
}

/// Whether `name` (an agent, user, or instance name) is safe as a single path component: non-empty,
/// no `/` or NUL, and not `.` or `..`.
pub fn is_valid_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    !name.contains('\0')
        && matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(c)), None) if c.as_bytes() == name.as_bytes()
        )
}

/// The resolved data root and every path in its layout.
///
/// Path accessors do no I/O. Accessors that take a name return [`PathsError::InvalidName`] unless
/// it is a single path component ([`is_valid_name`]), so no name can escape its parent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRoot {
    root: PathBuf,
    socket_dir: PathBuf,
}

impl DataRoot {
    /// Builds the layout for `root`, choosing the socket directory: `<root>/run/` if every socket
    /// path there fits in `sun_path`, else `<temp_dir>/giles-<hash of root>/`.
    ///
    /// `root` is made absolute against the current directory and normalized lexically (repeated
    /// and trailing separators and `.` components dropped; `..` and symlinks kept), so `/a/b`,
    /// `/a/b/`, and `/a//b` are the same root. `temp_dir` is only used, and only checked, when the
    /// sockets need the fallback. No I/O beyond reading the current directory.
    ///
    /// # Errors
    ///
    /// [`PathsError::EmptyRoot`] if `root` is empty; [`PathsError::EmptyTempDir`] if the fallback
    /// is needed and `temp_dir` is empty; [`PathsError::Absolute`] if the current directory is
    /// unavailable; [`PathsError::SocketPathTooLong`] if neither socket directory fits.
    pub fn new(root: impl Into<PathBuf>, temp_dir: &Path) -> Result<Self, PathsError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(PathsError::EmptyRoot);
        }
        let root = normalize(root)?;
        let sockets_fit = |dir: &Path| SOCKET_NAMES.iter().all(|n| socket_path_fits(&dir.join(n)));

        let run_dir = root.join("run");
        let socket_dir = if sockets_fit(&run_dir) {
            run_dir
        } else {
            if temp_dir.as_os_str().is_empty() {
                return Err(PathsError::EmptyTempDir);
            }
            let fallback = normalize(temp_dir.to_path_buf())?.join(fallback_dir_name(&root));
            if !sockets_fit(&fallback) {
                return Err(PathsError::SocketPathTooLong {
                    run_socket: run_dir.join(ADMIN_SOCKET),
                    fallback_socket: fallback.join(ADMIN_SOCKET),
                });
            }
            fallback
        };
        Ok(Self { root, socket_dir })
    }

    /// Resolves the data root from the process environment: `explicit` (the `--home` flag), then
    /// `GILES_HOME`, then `$HOME/.giles`, with the socket fallback under `$TMPDIR` (`/tmp` if it is
    /// unset or empty).
    ///
    /// # Errors
    ///
    /// As [`resolve_root`] and [`DataRoot::new`].
    pub fn from_env(explicit: Option<&Path>) -> Result<Self, PathsError> {
        let giles_home = std::env::var_os(GILES_HOME_ENV);
        let home = std::env::var_os("HOME");
        let root = resolve_root(explicit, giles_home.as_deref(), home.as_deref())?;
        let temp_dir = std::env::temp_dir();
        if temp_dir.as_os_str().is_empty() {
            Self::new(root, Path::new("/tmp"))
        } else {
            Self::new(root, &temp_dir)
        }
    }

    /// The data root itself (absolute).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `giles.toml`: core settings, instances, routes, and roles (section 11.2).
    pub fn config_file(&self) -> PathBuf {
        self.root.join("giles.toml")
    }

    /// `secrets.toml`, mode 0600 (section 11.2).
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.toml")
    }

    /// `agents/`: one directory per agent (section 11.3).
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// `agents/<agent>/`.
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `agent` is not a valid name ([`is_valid_name`]).
    pub fn agent_dir(&self, agent: &str) -> Result<PathBuf, PathsError> {
        Ok(self.agents_dir().join(checked_name(agent)?))
    }

    /// `users/`: one directory per user (section 11.3).
    pub fn users_dir(&self) -> PathBuf {
        self.root.join("users")
    }

    /// `users/<user>/`.
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `user` is not a valid name ([`is_valid_name`]).
    pub fn user_dir(&self, user: &str) -> Result<PathBuf, PathsError> {
        Ok(self.users_dir().join(checked_name(user)?))
    }

    /// `skills/`: global skills, `skills/<skill>/SKILL.md` (section 6.6).
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// `plugins/`: plugin binaries, searched before `$PATH` (section 7.3).
    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// `data/`: state the core writes.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// `data/giles.db`: the SQLite database (section 11.4).
    pub fn database_file(&self) -> PathBuf {
        self.data_dir().join("giles.db")
    }

    /// `data/blobs/`: the content-addressed blob store (section 11.5).
    pub fn blobs_dir(&self) -> PathBuf {
        self.data_dir().join("blobs")
    }

    /// `data/plugins/`: the parent of every instance's data directory.
    pub fn plugin_data_root(&self) -> PathBuf {
        self.data_dir().join("plugins")
    }

    /// `data/plugins/<instance>/`: the instance's data directory, its only writable directory
    /// (section 11.6).
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `instance` is not a valid name ([`is_valid_name`]).
    pub fn plugin_data_dir(&self, instance: &str) -> Result<PathBuf, PathsError> {
        Ok(self.plugin_data_root().join(checked_name(instance)?))
    }

    /// `backups/`: `giles backup` output and pre-migration snapshots (section 11.4).
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// `run/`: runtime files, mode 0700.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `run/admin.token`, mode 0600: the admin service's bearer token (section 8.7). It stays in
    /// `run/` even when the sockets fall back to `$TMPDIR`.
    pub fn admin_token_file(&self) -> PathBuf {
        self.run_dir().join("admin.token")
    }

    /// `run/giles.lock`, mode 0600: the running core holds an exclusive `flock` on it, so only
    /// one core uses a data root at a time. It holds that core's pid.
    pub fn lock_file(&self) -> PathBuf {
        self.run_dir().join("giles.lock")
    }

    /// `run/pids/`: plugin pid files (section 7.4).
    pub fn pids_dir(&self) -> PathBuf {
        self.run_dir().join("pids")
    }

    /// `run/pids/<instance>.pid`.
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `instance` is not a valid name ([`is_valid_name`]).
    pub fn pid_file(&self, instance: &str) -> Result<PathBuf, PathsError> {
        let instance = checked_name(instance)?;
        Ok(self.pids_dir().join(format!("{instance}.pid")))
    }

    /// `run/sandbox/`: the sandbox specs the plugin host writes for the trampoline (section 12.1).
    pub fn sandbox_specs_dir(&self) -> PathBuf {
        self.run_dir().join("sandbox")
    }

    /// `run/sandbox/<instance>.json`, mode 0600.
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `instance` is not a valid name ([`is_valid_name`]).
    pub fn sandbox_spec_file(&self, instance: &str) -> Result<PathBuf, PathsError> {
        let instance = checked_name(instance)?;
        Ok(self.sandbox_specs_dir().join(format!("{instance}.json")))
    }

    /// `cache/`: the Describe and tool-list cache (section 7.9).
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory holding the Unix sockets: `run/`, or the `$TMPDIR` fallback when paths under
    /// `run/` would be too long.
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    /// Whether the sockets use the `$TMPDIR` fallback instead of `run/`.
    pub fn uses_socket_fallback(&self) -> bool {
        self.socket_dir != self.run_dir()
    }

    /// The admin service socket, `run/admin.sock` or its fallback (section 8.7). Clients call
    /// [`DataRoot::check_socket_dir`] before connecting.
    pub fn admin_socket(&self) -> PathBuf {
        self.socket_dir.join(ADMIN_SOCKET)
    }

    /// The trusted-proxy listener socket, `run/proxy.sock` or its fallback (section 3.4). Clients
    /// call [`DataRoot::check_socket_dir`] before connecting.
    pub fn proxy_socket(&self) -> PathBuf {
        self.socket_dir.join(PROXY_SOCKET)
    }

    /// Creates the data root and its directories. Idempotent.
    ///
    /// Directories are created with mode 0700. Existing directories keep their modes, except
    /// `run/` and the socket directory, which are always set to 0700. A fallback socket directory
    /// must be a real directory with the same owner as `run/`; it is never followed through a
    /// symlink.
    ///
    /// # Errors
    ///
    /// [`PathsError::Io`] if a directory cannot be created or inspected (e.g. a file is in the
    /// way); [`PathsError::UnsafeSocketDir`] if the fallback socket directory is not ours.
    pub fn create_layout(&self) -> Result<(), PathsError> {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(PRIVATE_DIR_MODE);
        for dir in [
            self.root.clone(),
            self.agents_dir(),
            self.users_dir(),
            self.skills_dir(),
            self.plugins_dir(),
            self.data_dir(),
            self.blobs_dir(),
            self.plugin_data_root(),
            self.backups_dir(),
            self.run_dir(),
            self.pids_dir(),
            self.sandbox_specs_dir(),
            self.cache_dir(),
        ] {
            builder.create(&dir).map_err(|source| PathsError::Io {
                action: "create directory",
                path: dir,
                source,
            })?;
        }

        set_private(&self.run_dir())?;
        if self.uses_socket_fallback() {
            create_fallback_socket_dir(self)?;
        }
        Ok(())
    }

    /// Checks that the socket directory is safe to connect through. `run/` is inside the data root
    /// and needs no check. The `$TMPDIR` fallback must be a real directory (not a symlink) owned
    /// by the owner of `run/` and closed to other users (as [`DataRoot::create_layout`] leaves
    /// it).
    ///
    /// Clients must call this before connecting to [`DataRoot::admin_socket`] or
    /// [`DataRoot::proxy_socket`]: the fallback's name is predictable in a directory other users
    /// can write to, and a socket planted there would receive the admin token.
    ///
    /// # Errors
    ///
    /// [`PathsError::UnsafeSocketDir`] if the directory fails a check; [`PathsError::Io`] if it or
    /// `run/` cannot be inspected (e.g. the daemon never created them).
    pub fn check_socket_dir(&self) -> Result<(), PathsError> {
        if !self.uses_socket_fallback() {
            return Ok(());
        }
        let meta = inspect_fallback_socket_dir(self)?;
        if meta.mode() & 0o077 != 0 {
            return Err(unsafe_socket_dir(
                self.socket_dir(),
                "accessible by other users",
            ));
        }
        Ok(())
    }
}

/// Creates (or adopts) the `$TMPDIR` socket directory and makes it private.
fn create_fallback_socket_dir(root: &DataRoot) -> Result<(), PathsError> {
    let dir = root.socket_dir();
    match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(PathsError::Io {
                action: "create directory",
                path: dir.to_path_buf(),
                source,
            });
        }
    }
    inspect_fallback_socket_dir(root)?;
    set_private(dir)
}

/// The fallback socket directory's metadata, if it is a real directory (never followed through a
/// symlink) with the same owner as `run/`. Its parent is shared with other users and its name is
/// predictable, so anything else may be someone else's.
fn inspect_fallback_socket_dir(root: &DataRoot) -> Result<fs::Metadata, PathsError> {
    let dir = root.socket_dir();
    let owner = metadata(&root.run_dir())?.uid();
    let meta = fs::symlink_metadata(dir).map_err(|source| PathsError::Io {
        action: "inspect",
        path: dir.to_path_buf(),
        source,
    })?;
    if !meta.file_type().is_dir() {
        return Err(unsafe_socket_dir(dir, "not a directory"));
    }
    if meta.uid() != owner {
        return Err(unsafe_socket_dir(dir, "owned by another user"));
    }
    Ok(meta)
}

fn unsafe_socket_dir(dir: &Path, reason: &'static str) -> PathsError {
    PathsError::UnsafeSocketDir {
        path: dir.to_path_buf(),
        reason,
    }
}

fn set_private(dir: &Path) -> Result<(), PathsError> {
    fs::set_permissions(dir, Permissions::from_mode(PRIVATE_DIR_MODE)).map_err(|source| {
        PathsError::Io {
            action: "set permissions on",
            path: dir.to_path_buf(),
            source,
        }
    })
}

fn metadata(path: &Path) -> Result<fs::Metadata, PathsError> {
    fs::metadata(path).map_err(|source| PathsError::Io {
        action: "inspect",
        path: path.to_path_buf(),
        source,
    })
}

/// Makes `path` absolute and lexically normal: no repeated or trailing separators and no interior
/// `.`, so every spelling of a directory hashes to the same fallback socket directory. `..` is kept,
/// because removing it changes the meaning of a path that crosses a symlink.
fn normalize(path: PathBuf) -> Result<PathBuf, PathsError> {
    let path = if path.is_absolute() {
        path
    } else {
        std::path::absolute(&path).map_err(|source| PathsError::Absolute { path, source })?
    };
    Ok(path.components().collect())
}

fn checked_name(name: &str) -> Result<&str, PathsError> {
    if is_valid_name(name) {
        Ok(name)
    } else {
        Err(PathsError::InvalidName {
            name: name.to_owned(),
        })
    }
}

/// `giles-<16 hex digits>`: a stable name for the root's fallback socket directory.
fn fallback_dir_name(root: &Path) -> String {
    format!("giles-{:016x}", fnv1a64(root.as_os_str().as_bytes()))
}

/// 64-bit FNV-1a. Stable across Rust releases (unlike `DefaultHasher`), so a newer CLI finds an
/// older daemon's socket.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// An absolute root whose `run/admin.sock` path is exactly `len` bytes long.
    fn root_with_socket_len(len: usize) -> PathBuf {
        let suffix = "/run/admin.sock".len();
        PathBuf::from(format!("/{}", "r".repeat(len - suffix - 1)))
    }

    #[test]
    fn explicit_root_beats_env_and_home() {
        let root = resolve_root(
            Some(Path::new("/flag")),
            Some(OsStr::new("/env")),
            Some(OsStr::new("/home/me")),
        );
        assert_eq!(root.unwrap(), Path::new("/flag"));
    }

    #[test]
    fn giles_home_beats_home() {
        let root = resolve_root(None, Some(OsStr::new("/env")), Some(OsStr::new("/home/me")));
        assert_eq!(root.unwrap(), Path::new("/env"));
    }

    #[test]
    fn default_is_dot_giles_under_home() {
        let root = resolve_root(None, None, Some(OsStr::new("/home/me")));
        assert_eq!(root.unwrap(), Path::new("/home/me/.giles"));
        let root = resolve_root(None, Some(OsStr::new("")), Some(OsStr::new("/home/me")));
        assert_eq!(root.unwrap(), Path::new("/home/me/.giles"));
    }

    #[test]
    fn no_home_is_an_error() {
        assert!(matches!(
            resolve_root(None, None, None),
            Err(PathsError::NoHome)
        ));
        assert!(matches!(
            resolve_root(None, Some(OsStr::new("")), Some(OsStr::new(""))),
            Err(PathsError::NoHome)
        ));
    }

    #[test]
    fn empty_explicit_root_is_an_error() {
        let root = resolve_root(Some(Path::new("")), None, Some(OsStr::new("/home/me")));
        assert!(matches!(root, Err(PathsError::EmptyRoot)));
    }

    #[test]
    fn relative_root_is_made_absolute() {
        let root = DataRoot::new("rel/./home/", Path::new("/tmp")).unwrap();
        assert!(root.root().is_absolute());
        assert!(root.root().ends_with("rel/home"));
        assert_eq!(
            root.root(),
            std::env::current_dir().unwrap().join("rel/home")
        );
    }

    #[test]
    fn spellings_of_one_root_are_one_root() {
        let long = "x".repeat(MAX_SOCKET_PATH_LEN);
        let roots: Vec<_> = [
            format!("/a/{long}"),
            format!("/a/{long}/"),
            format!("/a//{long}"),
            format!("/a/./{long}//"),
        ]
        .into_iter()
        .map(|root| DataRoot::new(root, Path::new("/tmp")).unwrap())
        .collect();
        assert!(roots[0].uses_socket_fallback());
        assert_eq!(roots[0].root(), Path::new(&format!("/a/{long}")));
        for r in &roots[1..] {
            assert_eq!(r.root(), roots[0].root());
            assert_eq!(r.socket_dir(), roots[0].socket_dir());
        }
        // `..` is kept: it may cross a symlink.
        let dotdot = DataRoot::new("/a/../b", Path::new("/tmp")).unwrap();
        assert_eq!(dotdot.root(), Path::new("/a/../b"));
    }

    #[test]
    fn layout_paths() {
        let r = DataRoot::new("/g", Path::new("/tmp")).unwrap();
        assert_eq!(r.config_file(), Path::new("/g/giles.toml"));
        assert_eq!(r.secrets_file(), Path::new("/g/secrets.toml"));
        assert_eq!(r.agent_dir("ada").unwrap(), Path::new("/g/agents/ada"));
        assert_eq!(r.user_dir("bob").unwrap(), Path::new("/g/users/bob"));
        assert_eq!(r.skills_dir(), Path::new("/g/skills"));
        assert_eq!(r.plugins_dir(), Path::new("/g/plugins"));
        assert_eq!(r.database_file(), Path::new("/g/data/giles.db"));
        assert_eq!(r.blobs_dir(), Path::new("/g/data/blobs"));
        assert_eq!(
            r.plugin_data_dir("irc").unwrap(),
            Path::new("/g/data/plugins/irc")
        );
        assert_eq!(r.backups_dir(), Path::new("/g/backups"));
        assert_eq!(r.admin_token_file(), Path::new("/g/run/admin.token"));
        assert_eq!(r.lock_file(), Path::new("/g/run/giles.lock"));
        assert_eq!(r.pid_file("irc").unwrap(), Path::new("/g/run/pids/irc.pid"));
        assert_eq!(
            r.sandbox_spec_file("irc").unwrap(),
            Path::new("/g/run/sandbox/irc.json")
        );
        assert_eq!(r.cache_dir(), Path::new("/g/cache"));
        assert_eq!(r.admin_socket(), Path::new("/g/run/admin.sock"));
        assert_eq!(r.proxy_socket(), Path::new("/g/run/proxy.sock"));
        assert!(!r.uses_socket_fallback());
    }

    #[test]
    fn name_validation() {
        for good in ["irc", "my-agent", "a.b", "..x", "é"] {
            assert!(is_valid_name(good), "{good:?}");
        }
        for bad in ["", ".", "..", "a/b", "/a", "a/", "a\0b"] {
            assert!(!is_valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn invalid_names_are_errors() {
        let r = DataRoot::new("/g", Path::new("/tmp")).unwrap();
        let invalid = |res: Result<PathBuf, PathsError>, bad: &str| {
            assert!(
                matches!(&res, Err(PathsError::InvalidName { name }) if name == bad),
                "{res:?}"
            );
        };
        invalid(r.plugin_data_dir("../../etc"), "../../etc");
        invalid(r.pid_file(".."), "..");
        invalid(r.agent_dir("a/b"), "a/b");
        invalid(r.user_dir(""), "");
    }

    #[test]
    fn platform_socket_limit() {
        #[cfg(target_os = "macos")]
        assert_eq!(MAX_SOCKET_PATH_LEN, 103);
        #[cfg(target_os = "linux")]
        assert_eq!(MAX_SOCKET_PATH_LEN, 107);
    }

    /// The limit agrees with what the platform actually binds.
    #[test]
    fn socket_limit_matches_bind() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().as_os_str().len() + 1;
        let at = |len: usize| dir.path().join("s".repeat(len - base));

        let longest = at(MAX_SOCKET_PATH_LEN);
        assert_eq!(longest.as_os_str().len(), MAX_SOCKET_PATH_LEN);
        assert!(socket_path_fits(&longest));
        UnixListener::bind(&longest).expect("longest allowed path binds");

        let too_long = at(MAX_SOCKET_PATH_LEN + 1);
        assert!(!socket_path_fits(&too_long));
        assert!(UnixListener::bind(&too_long).is_err());
    }

    #[test]
    fn socket_stays_in_run_at_the_limit() {
        let root = root_with_socket_len(MAX_SOCKET_PATH_LEN);
        let r = DataRoot::new(&root, Path::new("/tmp")).unwrap();
        assert!(!r.uses_socket_fallback());
        assert_eq!(r.admin_socket(), root.join("run/admin.sock"));
        assert_eq!(r.admin_socket().as_os_str().len(), MAX_SOCKET_PATH_LEN);
    }

    #[test]
    fn socket_falls_back_one_past_the_limit() {
        let root = root_with_socket_len(MAX_SOCKET_PATH_LEN + 1);
        let r = DataRoot::new(&root, Path::new("/tmp")).unwrap();
        assert!(r.uses_socket_fallback());
        let dir = format!("/tmp/giles-{:016x}", fnv1a64(root.as_os_str().as_bytes()));
        assert_eq!(r.socket_dir(), Path::new(&dir));
        assert_eq!(r.admin_socket(), Path::new(&dir).join("admin.sock"));
        assert_eq!(r.proxy_socket(), Path::new(&dir).join("proxy.sock"));
        // Only the sockets move.
        assert_eq!(r.admin_token_file(), root.join("run/admin.token"));
        assert_eq!(r.pids_dir(), root.join("run/pids"));
    }

    #[test]
    fn fallback_dir_is_stable_per_root() {
        let a = root_with_socket_len(200);
        let b = PathBuf::from(format!("{}x", a.display()));
        let tmp = Path::new("/tmp");
        let socket_dir = |root: &Path| DataRoot::new(root, tmp).unwrap().socket_dir().to_owned();
        assert_eq!(socket_dir(&a), socket_dir(&a));
        assert_ne!(socket_dir(&a), socket_dir(&b));
    }

    #[test]
    fn too_long_everywhere_is_an_error() {
        let root = root_with_socket_len(200);
        let tmp = PathBuf::from(format!("/{}", "t".repeat(200)));
        assert!(matches!(
            DataRoot::new(root, &tmp),
            Err(PathsError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn empty_temp_dir_is_ignored_unless_needed() {
        let r = DataRoot::new("/g", Path::new("")).unwrap();
        assert_eq!(r.admin_socket(), Path::new("/g/run/admin.sock"));
        assert!(matches!(
            DataRoot::new(root_with_socket_len(200), Path::new("")),
            Err(PathsError::EmptyTempDir)
        ));
    }

    #[test]
    fn empty_root_is_an_error() {
        assert!(matches!(
            DataRoot::new("", Path::new("/tmp")),
            Err(PathsError::EmptyRoot)
        ));
    }

    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn create_layout_creates_private_dirs_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let r = DataRoot::new(tmp.path().join("home"), tmp.path()).unwrap();
        r.create_layout().unwrap();
        r.create_layout().unwrap();

        for dir in [
            r.root().to_owned(),
            r.agents_dir(),
            r.users_dir(),
            r.skills_dir(),
            r.plugins_dir(),
            r.data_dir(),
            r.blobs_dir(),
            r.plugin_data_root(),
            r.backups_dir(),
            r.run_dir(),
            r.pids_dir(),
            r.sandbox_specs_dir(),
            r.cache_dir(),
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
            assert_eq!(mode(&dir), 0o700, "{}", dir.display());
        }
        // Files are written by people or later subsystems, not by layout creation.
        assert!(!r.config_file().exists());
        assert!(!r.database_file().exists());
    }

    #[test]
    fn create_layout_tightens_run_but_keeps_other_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let r = DataRoot::new(tmp.path().join("home"), tmp.path()).unwrap();
        r.create_layout().unwrap();
        fs::set_permissions(r.run_dir(), Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(r.agents_dir(), Permissions::from_mode(0o750)).unwrap();

        r.create_layout().unwrap();
        assert_eq!(mode(&r.run_dir()), 0o700);
        assert_eq!(mode(&r.agents_dir()), 0o750);
    }

    #[test]
    fn create_layout_fails_when_a_file_is_in_the_way() {
        let tmp = tempfile::tempdir().unwrap();
        let r = DataRoot::new(tmp.path().join("home"), tmp.path()).unwrap();
        fs::create_dir(r.root()).unwrap();
        fs::write(r.data_dir(), "").unwrap();
        let err = r.create_layout().unwrap_err();
        assert!(matches!(&err, PathsError::Io { path, .. } if *path == r.data_dir()));
    }

    /// A data root deep enough that its sockets fall back to `temp`.
    fn long_root(tmp: &Path, temp: &Path) -> DataRoot {
        let root = tmp.join("h".repeat(MAX_SOCKET_PATH_LEN));
        let r = DataRoot::new(root, temp).unwrap();
        assert!(r.uses_socket_fallback());
        r
    }

    #[test]
    fn create_layout_creates_private_fallback_socket_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        r.create_layout().unwrap();
        assert!(r.socket_dir().starts_with(temp.path()));
        assert_eq!(mode(r.socket_dir()), 0o700);
        // An existing fallback directory is adopted and tightened.
        fs::set_permissions(r.socket_dir(), Permissions::from_mode(0o777)).unwrap();
        r.create_layout().unwrap();
        assert_eq!(mode(r.socket_dir()), 0o700);
        UnixListener::bind(r.admin_socket()).expect("fallback socket binds");
    }

    #[test]
    fn create_layout_rejects_symlinked_fallback_socket_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, r.socket_dir()).unwrap();
        assert!(matches!(
            r.create_layout(),
            Err(PathsError::UnsafeSocketDir { .. })
        ));
        assert!(matches!(
            r.check_socket_dir(),
            Err(PathsError::UnsafeSocketDir { .. })
        ));
    }

    #[test]
    fn create_layout_rejects_file_as_fallback_socket_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        fs::write(r.socket_dir(), "").unwrap();
        assert!(matches!(
            r.create_layout(),
            Err(PathsError::UnsafeSocketDir { .. })
        ));
        assert!(matches!(
            r.check_socket_dir(),
            Err(PathsError::UnsafeSocketDir { .. })
        ));
    }

    #[test]
    fn check_socket_dir_accepts_what_create_layout_leaves() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        r.create_layout().unwrap();
        r.check_socket_dir().unwrap();
    }

    #[test]
    fn check_socket_dir_rejects_an_open_fallback_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        r.create_layout().unwrap();
        fs::set_permissions(r.socket_dir(), Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            r.check_socket_dir(),
            Err(PathsError::UnsafeSocketDir { .. })
        ));
    }

    #[test]
    fn check_socket_dir_rejects_a_missing_fallback_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let r = long_root(tmp.path(), temp.path());
        assert!(matches!(r.check_socket_dir(), Err(PathsError::Io { .. })));
    }

    #[test]
    fn check_socket_dir_trusts_run() {
        // `run/` is inside the data root; there is nothing to check, even before it exists.
        let r = DataRoot::new("/nonexistent/giles", Path::new("/tmp")).unwrap();
        assert!(!r.uses_socket_fallback());
        r.check_socket_dir().unwrap();
    }
}
