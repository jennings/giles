//! The `giles sandbox-exec` trampoline (PLAN section 12).
//!
//! The core never sandboxes its own multi-threaded process. It launches every plugin through
//! `giles sandbox-exec <spec> -- <command> [args...]`, a fresh single-threaded process that reads
//! the [`Spec`], builds the command's [`scrubbed_env`], applies the platform sandbox, and `exec`s
//! the command in place, so stdin, stdout, stderr, and fd 3 (the plugin's end of the socket pair)
//! pass through unchanged. See `Docs/technical/sandbox.md`.
//!
//! The platform backends sit behind the internal [`backend::SandboxBackend`] trait: Landlock and
//! seccomp on Linux, Seatbelt on macOS. A backend that can't
//! apply its sandbox fails, and the trampoline runs nothing, unless the spec opts out with
//! [`Spec::unsandboxed`].
//!
//! [`check_host`] is the startup check that the host can sandbox at all.

mod backend;
mod env;
mod host;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod roots;
mod spec;

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use tracing::warn;

use self::backend::{Launch, Platform, SandboxBackend};
pub use self::env::scrubbed_env;
pub use self::host::{HostError, HostReport};
pub use self::spec::{
    DEFAULT_PATH, DERIVED_ENV, Network, PLUGIN_DATA_ENV, Spec, SpecError, TMP_DIR_NAME,
};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("the plugin sandbox supports only Linux and macOS");

/// Mode of the command's temporary directory when the trampoline creates it.
const TMP_DIR_MODE: u32 = 0o700;

/// The trampoline's exit status when it fails before `exec` (bad spec, data directory, sandbox,
/// or logging setup), like `env(1)` and `chroot(1)`. Retrying won't help.
pub const EXIT_TRAMPOLINE_FAILED: u8 = 125;

/// The trampoline's exit status when the command exists but can't be executed.
pub const EXIT_CANNOT_EXECUTE: u8 = 126;

/// The trampoline's exit status when the command doesn't exist.
pub const EXIT_NOT_FOUND: u8 = 127;

/// macOS `sandbox-exec`'s exit status when it can't apply the profile or `exec` the command
/// (`EX_OSERR`; see `Docs/technical/sandbox.md`), treated like [`EXIT_TRAMPOLINE_FAILED`].
pub const EXIT_SANDBOX_EXEC_FAILED: u8 = 71;

/// Whether `status`, of a command launched through the trampoline, means the trampoline failed
/// before running it: [`EXIT_TRAMPOLINE_FAILED`], or on macOS [`EXIT_SANDBOX_EXEC_FAILED`].
/// Retrying won't help until the configuration or the host changes. A command that itself exits
/// with these is indistinguishable, as with `env(1)`.
pub fn is_trampoline_failure(status: ExitStatus) -> bool {
    status.code().is_some_and(|code| {
        code == i32::from(EXIT_TRAMPOLINE_FAILED)
            || cfg!(target_os = "macos") && code == i32::from(EXIT_SANDBOX_EXEC_FAILED)
    })
}

/// Why the trampoline didn't run the command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// The spec is invalid.
    #[error(transparent)]
    Spec(#[from] SpecError),
    /// Nothing follows `--`.
    #[error("no command to run")]
    NoCommand,
    /// The command is not an absolute path. The core resolves it (with the operator's `PATH`)
    /// before launching, so the sandbox grants and the program run are the same file.
    #[error("the command {} is not an absolute path", .0.display())]
    RelativeCommand(PathBuf),
    /// The data directory is missing or not a directory.
    #[error("unusable data directory {}", .path.display())]
    DataDir {
        /// The data directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The data directory is a symbolic link. The command can write its data directory, so a
    /// link there may be one it planted to point its next launch somewhere else.
    #[error(
        "the data directory {} is a symbolic link; refusing to follow it (the plugin could have \
         replaced its data directory with a link to gain access elsewhere)",
        .0.display()
    )]
    DataDirLink(PathBuf),
    /// The writable grants (the data directory and `write` grants) keep resolving through a
    /// symbolic link inside one of them: links there are changing while the trampoline resolves
    /// them, which only the plugin (or a child it left behind) should be able to do.
    #[error(
        "the writable sandbox grant {} keeps resolving through the symbolic link {}, which is \
         inside a writable grant; refusing to launch (the plugin may be changing it)",
        .path.display(),
        .link.display()
    )]
    WritableLink {
        /// The path as the spec spells it.
        path: PathBuf,
        /// Where the link is.
        link: PathBuf,
    },
    /// The temporary directory inside the data directory can't be created.
    #[error("cannot create the temporary directory {}", .path.display())]
    TmpDir {
        /// The temporary directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The host can't apply the sandbox (on Linux: Landlock is missing or disabled). The
    /// trampoline fails closed instead of running the command unsandboxed.
    #[error(
        "cannot sandbox the command on this host, so it was not run (an instance can opt out \
         with `unsandboxed = true`)"
    )]
    Host(#[source] HostError),
    /// The kernel's Landlock ABI (capped at the newest Giles knows) is below the spec's
    /// `min_abi`.
    #[error(
        "the spec requires Landlock ABI {required} (`min_abi`), but this kernel provides ABI \
         {available}; refusing to run the command"
    )]
    LandlockAbi {
        /// The spec's `min_abi`.
        required: u32,
        /// The effective ABI.
        available: u32,
    },
    /// Building or applying the Landlock ruleset failed.
    #[error("cannot apply the Landlock ruleset")]
    Landlock(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Building or installing the seccomp filter failed.
    #[error("cannot apply the seccomp filter")]
    Seccomp(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// `exec` failed.
    #[error("cannot run {}", .program.display())]
    Exec {
        /// The program.
        program: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl SandboxError {
    /// The trampoline's exit status for this failure: [`EXIT_NOT_FOUND`] or
    /// [`EXIT_CANNOT_EXECUTE`] when `exec` failed, else [`EXIT_TRAMPOLINE_FAILED`].
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Exec { source, .. } if source.kind() == io::ErrorKind::NotFound => EXIT_NOT_FOUND,
            Self::Exec { .. } => EXIT_CANNOT_EXECUTE,
            _ => EXIT_TRAMPOLINE_FAILED,
        }
    }
}

/// Checks that this host can apply the sandbox (PLAN section 12.1): on Linux, that Landlock is
/// enabled and seccomp supports filters; on macOS, that `/usr/bin/sandbox-exec` is present.
///
/// Cheap and side-effect free. `serve` calls it at startup and logs the result; the plugin host
/// should refuse to launch sandboxed instances (reporting this one error) when it fails.
pub fn check_host() -> Result<HostReport, HostError> {
    Platform::check_host()
}

/// Runs `command` (program and arguments) under `spec`, replacing the current process.
///
/// Returns only on failure, like [`CommandExt::exec`]; on success the command is running in this
/// process. The caller must be single-threaded: Landlock and seccomp restrict only the calling
/// thread, and a forked-off thread would escape them. File descriptors are left alone, so
/// whatever the caller inherited without `FD_CLOEXEC` (stdin, stdout, stderr, fd 3) reaches the
/// command.
pub fn exec(spec: &Spec, command: &[OsString]) -> SandboxError {
    let (mut cmd, program) = match prepare(spec, command) {
        Ok(prepared) => prepared,
        Err(err) => return err,
    };
    SandboxError::Exec {
        program,
        source: cmd.exec(),
    }
}

/// Everything [`exec`] does short of `exec` itself, including applying the sandbox to this
/// process. Returns the command to run and its program, for error messages.
fn prepare(spec: &Spec, command: &[OsString]) -> Result<(Command, PathBuf), SandboxError> {
    spec.validate()?;
    let (program, args) = command.split_first().ok_or(SandboxError::NoCommand)?;
    let program = Path::new(program);
    if !program.is_absolute() {
        return Err(SandboxError::RelativeCommand(program.to_owned()));
    }
    prepare_data_dir(spec)?;
    let env = scrubbed_env(spec, &|name| std::env::var_os(name));

    let launch = Launch {
        program: program.to_owned(),
        args: args.to_vec(),
    };
    let launch = if spec.unsandboxed {
        warn!(
            program = %launch.program.display(),
            "running unsandboxed: the spec sets `unsandboxed`, so the command gets no filesystem, \
             network, or syscall restrictions",
        );
        launch
    } else {
        Platform::apply(spec, launch)?
    };

    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .env_clear()
        .envs(env)
        .current_dir(&spec.data_dir);
    Ok((cmd, launch.program))
}

/// Checks that the data directory is a directory, not a symbolic link, and creates the temporary
/// directory in it.
///
/// The command can write its data directory, including replacing it with a link, so following a
/// link there would let it choose what its next launch may write (on macOS the backend also
/// forbids removing the directory).
fn prepare_data_dir(spec: &Spec) -> Result<(), SandboxError> {
    let data_dir_error = |source| SandboxError::DataDir {
        path: spec.data_dir.clone(),
        source,
    };
    // Without a trailing `/`, which would make `lstat` follow a final link.
    let data_dir: PathBuf = spec.data_dir.components().collect();
    let meta = fs::symlink_metadata(&data_dir).map_err(data_dir_error)?;
    if meta.file_type().is_symlink() {
        return Err(SandboxError::DataDirLink(spec.data_dir.clone()));
    }
    if !meta.is_dir() {
        return Err(data_dir_error(io::ErrorKind::NotADirectory.into()));
    }
    let tmp = spec.tmp_dir();
    match DirBuilder::new().mode(TMP_DIR_MODE).create(&tmp) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && tmp.is_dir() => Ok(()),
        Err(source) => Err(SandboxError::TmpDir { path: tmp, source }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn command(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_must_be_absolute_and_present() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            unsandboxed: true,
            ..Spec::new(dir.path())
        };
        assert!(matches!(prepare(&spec, &[]), Err(SandboxError::NoCommand)));
        let err = prepare(&spec, &command(&["sh", "-c", "true"])).unwrap_err();
        assert_eq!(err.to_string(), "the command sh is not an absolute path");
    }

    #[test]
    fn invalid_spec_is_rejected_before_anything_else() {
        let spec = Spec {
            unsandboxed: true,
            ..Spec::new("relative")
        };
        assert!(matches!(
            prepare(&spec, &command(&["/bin/true"])),
            Err(SandboxError::Spec(SpecError::Path {
                field: "data_dir",
                ..
            }))
        ));
    }

    #[test]
    fn data_dir_must_not_be_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::create_dir(&victim).unwrap();
        let link = dir.path().join("data");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        for spelled in [link.clone(), link.join("")] {
            let err = prepare_data_dir(&Spec::new(&spelled)).unwrap_err();
            assert!(matches!(err, SandboxError::DataDirLink(_)), "{err}");
            assert_eq!(err.exit_code(), EXIT_TRAMPOLINE_FAILED);
        }
        // Nothing was created where the link points.
        assert!(!victim.join(TMP_DIR_NAME).exists());
    }

    #[test]
    fn data_dir_must_exist_and_tmp_is_created_private() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Spec::new(dir.path().join("missing"));
        assert!(matches!(
            prepare_data_dir(&missing),
            Err(SandboxError::DataDir { .. })
        ));
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let err = prepare_data_dir(&Spec::new(&file)).unwrap_err();
        assert!(matches!(&err, SandboxError::DataDir { source, .. }
            if source.kind() == io::ErrorKind::NotADirectory));

        let spec = Spec::new(dir.path());
        prepare_data_dir(&spec).unwrap();
        let mode = fs::metadata(spec.tmp_dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, TMP_DIR_MODE);
        // An existing temporary directory is reused.
        prepare_data_dir(&spec).unwrap();
    }

    #[test]
    fn unsandboxed_command_gets_the_scrubbed_env_and_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            unsandboxed: true,
            ..Spec::new(dir.path())
        };
        let (cmd, program) = prepare(&spec, &command(&["/bin/sh", "-c", "true"])).unwrap();
        assert_eq!(program, Path::new("/bin/sh"));
        assert_eq!(cmd.get_program(), "/bin/sh");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-c", "true"]);
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
        // `env_clear` plus exactly the scrubbed variables.
        let envs: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.unwrap().to_owned()))
            .collect();
        assert_eq!(envs, scrubbed_env(&spec, &|name| std::env::var_os(name)));
    }

    /// Without Landlock, or below `min_abi`, nothing is restricted and nothing runs. (A launch
    /// that succeeds would restrict this test process; `crates/giles/tests/sandbox_linux.rs`
    /// runs those through the real trampoline.)
    #[cfg(target_os = "linux")]
    #[test]
    fn sandboxed_launch_fails_closed_before_restricting_anything() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            min_abi: Some(1000),
            ..Spec::new(dir.path())
        };
        let err = prepare(&spec, &command(&["/bin/sh"])).unwrap_err();
        match check_host() {
            Ok(HostReport::Linux { landlock_abi }) => assert!(
                matches!(err, SandboxError::LandlockAbi { required: 1000, available }
                    if available == landlock_abi),
                "{err}"
            ),
            Ok(other) => panic!("{other:?}"),
            Err(_) => assert!(matches!(err, SandboxError::Host(_)), "{err}"),
        }
        assert_eq!(err.exit_code(), EXIT_TRAMPOLINE_FAILED);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_launch_runs_through_sandbox_exec() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec::new(dir.path());
        let (cmd, program) = prepare(&spec, &command(&["/bin/sh", "-c", "true"])).unwrap();
        assert_eq!(program, Path::new("/usr/bin/sandbox-exec"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args[0], "-p");
        assert_eq!(args[2..], ["/bin/sh", "-c", "true"]);
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
        let envs: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.unwrap().to_owned()))
            .collect();
        assert_eq!(envs, scrubbed_env(&spec, &|name| std::env::var_os(name)));
    }

    #[test]
    fn exit_codes_follow_env() {
        let exec = |kind: io::ErrorKind| SandboxError::Exec {
            program: "/p".into(),
            source: kind.into(),
        };
        assert_eq!(exec(io::ErrorKind::NotFound).exit_code(), EXIT_NOT_FOUND);
        assert_eq!(
            exec(io::ErrorKind::PermissionDenied).exit_code(),
            EXIT_CANNOT_EXECUTE
        );
        assert_eq!(
            SandboxError::RelativeCommand("p".into()).exit_code(),
            EXIT_TRAMPOLINE_FAILED
        );
        assert_eq!(
            SandboxError::from(SpecError::MinAbi).exit_code(),
            EXIT_TRAMPOLINE_FAILED
        );
        assert_eq!(
            (EXIT_TRAMPOLINE_FAILED, EXIT_CANNOT_EXECUTE, EXIT_NOT_FOUND),
            (125, 126, 127)
        );
    }

    #[test]
    fn trampoline_failures_are_125_and_on_macos_71() {
        use std::os::unix::process::ExitStatusExt as _;
        let exit = |code: i32| std::process::ExitStatus::from_raw(code << 8);
        assert!(is_trampoline_failure(exit(125)));
        assert_eq!(is_trampoline_failure(exit(71)), cfg!(target_os = "macos"));
        for code in [0, 1, 126, 127] {
            assert!(!is_trampoline_failure(exit(code)), "{code}");
        }
        // Killed by a signal: no exit code.
        assert!(!is_trampoline_failure(std::process::ExitStatus::from_raw(
            9
        )));
    }
}
