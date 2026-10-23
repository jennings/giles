//! The Linux backend: Landlock and seccomp (PLAN section 12.2).

use super::backend::{Launch, SandboxBackend};
use super::{SandboxError, Spec};

/// Landlock for the filesystem and TCP ports, a seccomp allowlist for syscalls and socket
/// families.
pub(crate) struct Linux;

impl SandboxBackend for Linux {
    /// Staged: TASKS.md 2.13 builds the Landlock ruleset (newest ABI, `min_abi` enforced) and the
    /// seccomp filter. Until then this fails closed.
    fn apply(_spec: &Spec, _launch: Launch) -> Result<Launch, SandboxError> {
        Err(SandboxError::NotImplemented {
            sandbox: "Linux (Landlock and seccomp)",
            task: "2.13",
        })
    }
}
