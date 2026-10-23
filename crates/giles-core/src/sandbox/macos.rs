//! The macOS backend: a generated Seatbelt profile applied through `sandbox-exec` (PLAN section
//! 12.3).

use super::backend::{Launch, SandboxBackend};
use super::{SandboxError, Spec};

/// A deny-by-default Seatbelt profile, applied by wrapping the command in `sandbox-exec`.
pub(crate) struct Macos;

impl SandboxBackend for Macos {
    /// Staged: TASKS.md 2.14 generates the profile and wraps the launch in `sandbox-exec`. Until
    /// then this fails closed.
    fn apply(_spec: &Spec, _launch: Launch) -> Result<Launch, SandboxError> {
        Err(SandboxError::NotImplemented {
            sandbox: "macOS (Seatbelt)",
            task: "2.14",
        })
    }
}
