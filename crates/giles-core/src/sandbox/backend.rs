//! The internal platform-backend trait, so another platform can be added without touching the
//! trampoline or the plugin contracts (PLAN section 12).

use std::ffi::OsString;
use std::path::PathBuf;

use super::host::{HostError, HostReport};
use super::{SandboxError, Spec};

/// The program the trampoline `exec`s and its arguments (without `argv[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    /// Absolute path of the program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
}

/// A platform's sandbox.
pub(crate) trait SandboxBackend {
    /// Probes whether this host can apply the sandbox at all, without changing anything. Cheap;
    /// [`super::check_host`] calls it at `serve` startup. Returns the first missing piece.
    fn check_host() -> Result<HostReport, HostError>;

    /// Applies the sandbox for `spec` and returns what to `exec`.
    ///
    /// Runs in the single-threaded trampoline, after the spec is validated, the data directory is
    /// checked, and its temporary directory exists, and right before `exec` with the scrubbed
    /// environment and the data directory as the working directory. A backend either restricts
    /// the current process and returns `launch` unchanged (Linux: Landlock, then seccomp, which
    /// survive `exec`), or returns a wrapped launch (macOS: `sandbox-exec -p <profile>
    /// <program> <args>`). On error nothing is run.
    fn apply(spec: &Spec, launch: Launch) -> Result<Launch, SandboxError>;
}

/// This platform's backend.
#[cfg(target_os = "linux")]
pub(crate) type Platform = super::linux::Linux;
/// This platform's backend.
#[cfg(target_os = "macos")]
pub(crate) type Platform = super::macos::Macos;
