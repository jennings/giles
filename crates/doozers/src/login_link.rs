//! `doozers admin login-link [--user <user>]`: a one-time recovery URL that signs a user in to the
//! web chat, from the admin service's `CreateLoginLink`.

use std::path::Path;

use doozers_proto::admin::v1 as pb;
use jiff::tz::TimeZone;

use crate::admin_client::{self, proto_time};
use crate::cli::LoginLinkArgs;

/// Prints the URL on standard output (alone, for scripts), and on standard error whose link it is
/// and when it expires.
pub fn run(home: Option<&Path>, args: &LoginLinkArgs) -> anyhow::Result<()> {
    let user = args.user.clone().unwrap_or_default();
    let link = admin_client::call(home, async |mut client| {
        let request = pb::CreateLoginLinkRequest { user };
        Ok(client.create_login_link(request).await?.into_inner())
    })?;
    println!("{}", link.url);
    eprintln!("The link logs in {} (id {}).", link.username, link.user);
    if let Some(expires_at) = link.expires_at {
        eprintln!(
            "The link works once, until {}.",
            proto_time(expires_at, &TimeZone::system())
        );
    }
    Ok(())
}
