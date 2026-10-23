//! The sandbox spec for one plugin launch: the instance's grants from `giles.toml` plus PLAN
//! section 12.4's defaults, and the handshake environment of section 7.4.

use std::path::Path;

use giles_proto::handshake::{MAGIC_COOKIE, MAGIC_COOKIE_ENV, PROTOCOL_MAJOR, PROTOCOLS_ENV};

use crate::config::{NetworkGrant, Sandbox};
use crate::sandbox::{Network, Spec};

/// The protocol majors the core offers in `GILES_PLUGIN_PROTOCOLS`.
pub(crate) const OFFERED_PROTOCOLS: [u32; 1] = [PROTOCOL_MAJOR];

/// The spec for launching `binary` (absolute, symlinks resolved) with `data_dir` under `sandbox`.
///
/// Network comes from the grants, whose per-kind default (on for channels) the config loader
/// already applied. The binary itself is the only extra read grant: the backends always allow
/// reading and running the command, and its directory is deliberately not granted (a binary
/// placed in, say, a home directory would expose the whole directory).
pub(crate) fn launch_spec(data_dir: &Path, binary: &Path, sandbox: &Sandbox) -> Spec {
    let grants = &sandbox.grants;
    let mut spec = Spec::new(data_dir);
    spec.read = vec![binary.to_path_buf()];
    spec.read.extend(grants.read.iter().cloned());
    spec.write.clone_from(&grants.write);
    spec.unix.clone_from(&grants.unix);
    spec.network = match &grants.network {
        NetworkGrant::None => Network::Off,
        NetworkGrant::All => Network::All,
        NetworkGrant::TcpPorts(ports) => Network::Tcp(ports.iter().copied().collect()),
    };
    spec.min_abi = grants.min_abi;
    spec.unsandboxed = sandbox.unsandboxed;
    spec.env
        .insert(MAGIC_COOKIE_ENV.to_owned(), MAGIC_COOKIE.to_owned());
    spec.env
        .insert(PROTOCOLS_ENV.to_owned(), offered_protocols());
    spec
}

/// `GILES_PLUGIN_PROTOCOLS`: the offered majors, comma-separated.
fn offered_protocols() -> String {
    OFFERED_PROTOCOLS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::Grants;

    fn sandbox(unsandboxed: bool, grants: Grants) -> Sandbox {
        Sandbox {
            unsandboxed,
            grants,
        }
    }

    #[test]
    fn defaults_grant_only_the_binary_and_the_handshake_env() {
        let grants = Grants {
            network: NetworkGrant::All,
            ..Grants::default()
        };
        let spec = launch_spec(
            Path::new("/g/data/plugins/irc"),
            Path::new("/opt/bin/giles-channel-irc"),
            &sandbox(false, grants),
        );
        let mut expected = Spec::new("/g/data/plugins/irc");
        expected.read = vec![PathBuf::from("/opt/bin/giles-channel-irc")];
        expected.network = Network::All;
        expected.env = [
            (MAGIC_COOKIE_ENV.to_owned(), MAGIC_COOKIE.to_owned()),
            (PROTOCOLS_ENV.to_owned(), "1".to_owned()),
        ]
        .into();
        assert_eq!(spec, expected);
        spec.validate().unwrap();
    }

    #[test]
    fn grants_map_onto_the_spec() {
        let grants = Grants {
            network: NetworkGrant::TcpPorts(vec![443, 6697]),
            read: vec!["/opt/homebrew".into()],
            write: vec!["/Users/me/Notes".into()],
            unix: vec!["/run/agent.sock".into()],
            min_abi: Some(4),
        };
        let spec = launch_spec(
            Path::new("/g/data/plugins/irc"),
            Path::new("/b/giles-channel-irc"),
            &sandbox(false, grants),
        );
        assert_eq!(
            spec.read,
            [
                Path::new("/b/giles-channel-irc"),
                Path::new("/opt/homebrew")
            ]
        );
        assert_eq!(spec.write, [Path::new("/Users/me/Notes")]);
        assert_eq!(spec.unix, [Path::new("/run/agent.sock")]);
        assert_eq!(spec.network, Network::Tcp([443, 6697].into()));
        assert_eq!(spec.min_abi, Some(4));
        assert!(!spec.unsandboxed);
        assert!(spec.execute.is_empty());
    }

    #[test]
    fn no_network_grant_is_off_and_unsandboxed_carries_over() {
        let spec = launch_spec(
            Path::new("/d"),
            Path::new("/b/p"),
            &sandbox(true, Grants::default()),
        );
        assert_eq!(spec.network, Network::Off);
        assert!(spec.unsandboxed);
    }
}
