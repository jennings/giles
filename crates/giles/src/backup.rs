//! `giles backup <dir>`: a consistent snapshot of the data root, from the admin service's
//! `Backup` (PLAN section 11.4).

use std::path::Path;

use anyhow::Context;
use giles_proto::admin::v1 as pb;

use crate::admin_client;
use crate::cli::BackupArgs;

/// Asks the running daemon to write a backup into the directory and prints the files it wrote on
/// standard output, one per line.
pub fn run(home: Option<&Path>, args: &BackupArgs) -> anyhow::Result<()> {
    // The daemon resolves the path in its own working directory, so send an absolute one.
    let dir = std::path::absolute(&args.dir)
        .with_context(|| format!("cannot resolve {}", args.dir.display()))?;
    let dir = dir
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", dir.display()))?
        .to_owned();
    let response = admin_client::call(home, async |mut client| {
        Ok(client.backup(pb::BackupRequest { dir }).await?.into_inner())
    })?;
    for path in response.paths {
        println!("{path}");
    }
    Ok(())
}
