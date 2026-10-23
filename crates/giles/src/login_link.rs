//! `giles admin login-link --user <user>`: a one-time web login URL, from the admin service's
//! `CreateLoginLink`.

use std::path::Path;

use giles_proto::admin::v1 as pb;
use jiff::tz::TimeZone;

use crate::admin_client::{self, proto_time};
use crate::cli::LoginLinkArgs;

/// Prints the URL on standard output (alone, for scripts) and when it expires on standard error.
pub fn run(home: Option<&Path>, args: &LoginLinkArgs) -> anyhow::Result<()> {
    let user = args.user.clone();
    let link = admin_client::call(home, async |mut client| {
        let request = pb::CreateLoginLinkRequest { user };
        Ok(client.create_login_link(request).await?.into_inner())
    })?;
    println!("{}", link.url);
    if let Some(expires_at) = link.expires_at {
        eprintln!(
            "The link works once, until {}.",
            proto_time(expires_at, &TimeZone::system())
        );
    }
    Ok(())
}
