//! `giles-workspace-helper`: the static Linux helper the Docker environment plugin copies into each
//! workspace container at `/.giles/bin/giles-workspace-helper`.
//!
//! The plugin runs it with `docker exec`, so it must work in any image: a single static binary
//! with no dependency on the image's tools beyond `/bin/sh`.
//!
//! ```text
//! idle                                       sleep forever (the container's main process)
//! run --id ID -- COMMAND [ARG...]            run a command in its own process group
//! kill --id ID                               SIGKILL the process group of `run --id ID`
//! find --pattern GLOB [--path DIR] --limit N list files, as JSON lines
//! grep --pattern RE [--path P] [--glob GLOB] [--ignore-case] --limit N
//!                                            search file contents, as JSON lines
//! read [--max N] PATH                        a `{"size":N}` line, then up to N bytes of the file
//! write PATH                                 write stdin to the file, creating parent directories
//! df PATH                                    a `{"free_bytes":N}` line: space free where PATH is
//! ```
//!
//! Failures print a message to standard error and exit 1 (66 when a path doesn't exist), except
//! `run`, which exits with its command's status.

mod files;
mod find;
mod grep;
mod run;
mod walk;

use std::ffi::OsString;
use std::process::ExitCode;

/// The exit status for a path that doesn't exist (`EX_NOINPUT`).
const EXIT_NOT_FOUND: u8 = 66;
/// The exit status for a bad command line (`EX_USAGE`).
const EXIT_USAGE: u8 = 64;

/// A failure to report on standard error.
#[derive(Debug)]
struct Failure {
    message: String,
    code: u8,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_USAGE,
        }
    }

    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    /// Reports an I/O failure on `path`, with the not-found status for a missing path.
    fn io(path: &std::path::Path, error: &std::io::Error) -> Self {
        let code = if error.kind() == std::io::ErrorKind::NotFound {
            EXIT_NOT_FOUND
        } else {
            1
        };
        Self {
            message: format!("{}: {}", path.display(), describe_io(error)),
            code,
        }
    }
}

/// An I/O error without the "(os error N)" suffix.
fn describe_io(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

type Outcome<T> = Result<T, Failure>;

/// The arguments after the subcommand name.
struct Args(Vec<OsString>);

impl Args {
    /// Removes `--name VALUE` and returns the value.
    fn value(&mut self, name: &str) -> Outcome<Option<OsString>> {
        let Some(at) = self.0.iter().position(|a| a == name) else {
            return Ok(None);
        };
        if at + 1 >= self.0.len() {
            return Err(Failure::usage(format!("{name} needs a value")));
        }
        self.0.remove(at);
        Ok(Some(self.0.remove(at)))
    }

    /// Like [`Args::value`], as text.
    fn text(&mut self, name: &str) -> Outcome<Option<String>> {
        self.value(name)?
            .map(|v| {
                v.into_string()
                    .map_err(|_| Failure::usage(format!("{name} must be UTF-8")))
            })
            .transpose()
    }

    /// Like [`Args::text`], but the flag is required.
    fn required(&mut self, name: &str) -> Outcome<String> {
        self.text(name)?
            .ok_or_else(|| Failure::usage(format!("{name} is required")))
    }

    /// Removes a flag without a value and says whether it was there.
    fn flag(&mut self, name: &str) -> bool {
        match self.0.iter().position(|a| a == name) {
            Some(at) => {
                self.0.remove(at);
                true
            }
            None => false,
        }
    }

    /// Removes `--name N`.
    fn number(&mut self, name: &str) -> Outcome<Option<u64>> {
        self.text(name)?
            .map(|v| {
                v.parse()
                    .map_err(|_| Failure::usage(format!("{name} must be a number")))
            })
            .transpose()
    }

    /// Fails if anything is left over.
    fn finish(self) -> Outcome<()> {
        match self.0.first() {
            None => Ok(()),
            Some(extra) => Err(Failure::usage(format!(
                "unexpected argument {}",
                extra.to_string_lossy()
            ))),
        }
    }
}

fn dispatch(subcommand: &str, rest: Vec<OsString>) -> Outcome<ExitCode> {
    let mut args = Args(rest);
    match subcommand {
        "idle" => {
            args.finish()?;
            loop {
                std::thread::park();
            }
        }
        "run" => run::run(args),
        "kill" => {
            let id = args.required("--id")?;
            args.finish()?;
            run::kill(&id)?;
            Ok(ExitCode::SUCCESS)
        }
        "find" => {
            let pattern = args.required("--pattern")?;
            let path = args.value("--path")?;
            let limit = args.number("--limit")?;
            args.finish()?;
            find::run(
                &pattern,
                path.as_deref(),
                limit.ok_or_else(|| Failure::usage("--limit is required"))?,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        "grep" => {
            let pattern = args.required("--pattern")?;
            let path = args.value("--path")?;
            let glob = args.text("--glob")?;
            let ignore_case = args.flag("--ignore-case");
            let limit = args.number("--limit")?;
            args.finish()?;
            grep::run(&grep::Request {
                pattern: &pattern,
                path: path.as_deref(),
                glob: glob.as_deref(),
                ignore_case,
                limit: limit.ok_or_else(|| Failure::usage("--limit is required"))?,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        "read" => {
            let max = args.number("--max")?.unwrap_or(u64::MAX);
            let path = single_path(args)?;
            files::read(&path, max)?;
            Ok(ExitCode::SUCCESS)
        }
        "write" => {
            let path = single_path(args)?;
            files::write(&path)?;
            Ok(ExitCode::SUCCESS)
        }
        "df" => {
            let path = single_path(args)?;
            files::free_space(&path)?;
            Ok(ExitCode::SUCCESS)
        }
        other => Err(Failure::usage(format!("unknown subcommand {other:?}"))),
    }
}

fn single_path(args: Args) -> Outcome<OsString> {
    let mut rest = args.0;
    if rest.len() != 1 {
        return Err(Failure::usage("expected exactly one path"));
    }
    Ok(rest.remove(0))
}

fn main() -> ExitCode {
    let mut argv = std::env::args_os().skip(1);
    let Some(subcommand) = argv.next() else {
        eprintln!(
            "giles-workspace-helper: expected a subcommand (idle, run, kill, find, grep, read, write, df)"
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let subcommand = subcommand.to_string_lossy().into_owned();
    match dispatch(&subcommand, argv.collect()) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("giles-workspace-helper: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}
