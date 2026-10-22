//! The `tracing` subscriber: human-readable lines on stderr, filtered by `GILES_LOG` or `RUST_LOG`.

use std::io::{self, IsTerminal};

use anyhow::{Context, anyhow};
use tracing_subscriber::EnvFilter;

/// The environment variable holding the log filter. Wins over `RUST_LOG`.
const GILES_LOG_ENV: &str = "GILES_LOG";
/// The filter when neither `GILES_LOG` nor `RUST_LOG` is set.
const DEFAULT_FILTER: &str = "info";

/// Installs the global subscriber. Call once, before anything logs.
pub fn init() -> anyhow::Result<()> {
    let directives = filter_directives(
        std::env::var(GILES_LOG_ENV).ok(),
        std::env::var(EnvFilter::DEFAULT_ENV).ok(),
    );
    tracing_subscriber::fmt()
        .with_env_filter(parse_filter(&directives)?)
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .try_init()
        .map_err(|e| anyhow!(e))
        .context("cannot install the log subscriber")
}

/// The filter directives: `GILES_LOG`, else `RUST_LOG`, else [`DEFAULT_FILTER`]. Empty values count
/// as unset.
fn filter_directives(giles_log: Option<String>, rust_log: Option<String>) -> String {
    [giles_log, rust_log]
        .into_iter()
        .flatten()
        .find(|d| !d.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_FILTER.to_owned())
}

fn parse_filter(directives: &str) -> anyhow::Result<EnvFilter> {
    EnvFilter::try_new(directives)
        .with_context(|| format!("invalid log filter {directives:?} in GILES_LOG or RUST_LOG"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn giles_log_beats_rust_log() {
        let d = filter_directives(some("giles=debug"), some("warn"));
        assert_eq!(d, "giles=debug");
    }

    #[test]
    fn rust_log_is_used_without_giles_log() {
        assert_eq!(filter_directives(None, some("warn")), "warn");
        assert_eq!(filter_directives(some(" "), some("warn")), "warn");
    }

    #[test]
    fn default_is_info() {
        assert_eq!(filter_directives(None, None), "info");
        assert_eq!(filter_directives(some(""), some("")), "info");
    }

    #[test]
    fn invalid_filters_are_rejected() {
        assert!(parse_filter("giles=debug,warn").is_ok());
        let err = parse_filter("giles=loud").unwrap_err();
        assert!(format!("{err:#}").contains("GILES_LOG"), "{err:#}");
    }
}
