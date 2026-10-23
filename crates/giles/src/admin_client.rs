//! Calling the running daemon's admin service from a CLI command.

use std::path::Path;

use anyhow::{Context, anyhow};
use giles_core::admin::{self, AdminClient, ConnectError};
use giles_core::paths::DataRoot;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use tonic::{Code, Status};

/// Connects to the daemon running on the data root (`--home`, `GILES_HOME`, or `~/.giles`) and
/// runs `call` with the client, on a single-threaded runtime: CLI commands start no other
/// threads. Errors are worded for the operator.
pub fn call<T>(
    home: Option<&Path>,
    call: impl AsyncFnOnce(AdminClient) -> Result<T, Status>,
) -> anyhow::Result<T> {
    let root = DataRoot::from_env(home)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(async {
            let client = admin::connect(&root).await.map_err(connect_error)?;
            call(client)
                .await
                .map_err(|status| rpc_error(&status, &root))
        })
}

fn connect_error(err: ConnectError) -> anyhow::Error {
    match err {
        ConnectError::NotRunning { .. } => anyhow!("{err}; start it with `giles serve`"),
        err => err.into(),
    }
}

fn rpc_error(status: &Status, root: &DataRoot) -> anyhow::Error {
    match status.code() {
        Code::Unauthenticated => anyhow!(
            "giles rejected the admin token in {}; if it just restarted, try again",
            root.admin_token_file().display()
        ),
        _ => anyhow!("{}", status.message()),
    }
}

/// `at` in `tz`, to the second: `2026-10-23 14:03:12 PDT`.
pub fn local_time(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone())
        .strftime("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// A `google.protobuf.Timestamp` from the daemon in `tz`, or a placeholder if it's invalid.
pub fn proto_time(at: prost_types::Timestamp, tz: &TimeZone) -> String {
    giles_plugin_sdk::convert::timestamp_from_proto(at)
        .map_or_else(|_| "(invalid time)".to_owned(), |at| local_time(at, tz))
}
