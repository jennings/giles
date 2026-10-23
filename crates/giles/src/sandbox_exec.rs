//! `giles sandbox-exec`: the plugin sandbox trampoline (`giles_core::sandbox`).
//!
//! Must stay single-threaded: no async runtime and no threads, because Landlock and seccomp apply
//! only to the calling thread.

use giles_core::sandbox::{self, EXIT_TRAMPOLINE_FAILED, SandboxError, Spec};

use crate::cli::SandboxExecArgs;

/// Loads the spec and `exec`s the command in the sandbox. Never returns: on failure it prints the
/// error like `main` does and exits with the code from the exit-status contract
/// ([`SandboxError::exit_code`]; [`EXIT_TRAMPOLINE_FAILED`] if `logging` failed to set up), so the
/// core can tell the trampoline's failures from the plugin's own exit statuses.
pub fn run(args: &SandboxExecArgs, logging: anyhow::Result<()>) -> ! {
    if let Err(err) = logging {
        fail(&err, EXIT_TRAMPOLINE_FAILED);
    }
    let err = match Spec::load(&args.spec) {
        Ok(spec) => sandbox::exec(&spec, &args.command),
        Err(err) => SandboxError::from(err),
    };
    let code = err.exit_code();
    fail(&err.into(), code)
}

fn fail(err: &anyhow::Error, code: u8) -> ! {
    eprintln!("Error: {err:?}");
    std::process::exit(code.into())
}
