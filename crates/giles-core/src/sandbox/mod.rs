//! The `giles sandbox-exec` trampoline (PLAN section 12).
//!
//! The core never sandboxes its own multi-threaded process. It launches every plugin through
//! `giles sandbox-exec <spec> -- <command> [args...]`, a fresh single-threaded process that reads
//! the [`Spec`], builds the command's [`scrubbed_env`], applies the platform sandbox, and `exec`s
//! the command in place, so stdin, stdout, stderr, and fd 3 (the plugin's end of the socket pair)
//! pass through unchanged. See `Docs/technical/sandbox.md`.
//!
//! The platform backends sit behind the internal [`backend::SandboxBackend`] trait. They are
//! staged: until the Linux backend (Landlock and seccomp, TASKS.md 2.13) and the macOS backend
//! (Seatbelt, TASKS.md 2.14) land, applying either fails with [`SandboxError::NotImplemented`],
//! so the trampoline refuses to run anything unless the spec opts out with
//! [`Spec::unsandboxed`].

mod backend;
mod env;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod spec;

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::warn;

use self::backend::{Launch, Platform, SandboxBackend};
pub use self::env::scrubbed_env;
pub use self::spec::{DEFAULT_PATH, Network, PLUGIN_DATA_ENV, Spec, SpecError, TMP_DIR_NAME};

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
    /// The temporary directory inside the data directory can't be created.
    #[error("cannot create the temporary directory {}", .path.display())]
    TmpDir {
        /// The temporary directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// This platform's sandbox backend is not built yet (a staged implementation; see the
    /// module docs). The trampoline fails closed instead of running the command unsandboxed.
    #[error(
        "the {sandbox} sandbox is not implemented yet (TASKS.md {task}); refusing to run the \
         command without it (an instance can opt out with `unsandboxed = true`)"
    )]
    NotImplemented {
        /// The sandbox that's missing, e.g. "Linux (Landlock and seccomp)".
        sandbox: &'static str,
        /// The task that implements it.
        task: &'static str,
    },
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
    let env = scrubbed_env(spec, std::env::var_os("LANG").as_deref());

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

/// Checks that the data directory exists and creates the temporary directory in it.
fn prepare_data_dir(spec: &Spec) -> Result<(), SandboxError> {
    let data_dir_error = |source| SandboxError::DataDir {
        path: spec.data_dir.clone(),
        source,
    };
    let meta = fs::metadata(&spec.data_dir).map_err(data_dir_error)?;
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
        assert_eq!(
            envs,
            scrubbed_env(&spec, std::env::var_os("LANG").as_deref())
        );
    }

    #[test]
    fn sandboxed_launch_fails_closed_until_the_backend_lands() {
        let dir = tempfile::tempdir().unwrap();
        let err = prepare(&Spec::new(dir.path()), &command(&["/bin/sh"])).unwrap_err();
        assert!(matches!(err, SandboxError::NotImplemented { .. }), "{err}");
        assert!(err.to_string().contains("refusing to run"), "{err}");
        assert_eq!(err.exit_code(), EXIT_TRAMPOLINE_FAILED);
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
}
