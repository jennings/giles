//! `giles-environment-docker`: the Giles Docker environment plugin. Giles launches it (`socket`);
//! an operator can run it as a remote plugin (`serve`).

use std::io;
use std::process::ExitCode;

use giles_environment_docker::{Config, DockerEnvironment, Settings, default_helper_dir};
use giles_plugin_sdk::EnvironmentPlugin;
use giles_plugin_sdk::Grants;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("GILES_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    EnvironmentPlugin::new("docker", env!("CARGO_PKG_VERSION"))
        .requested_grants(Grants {
            unix: default_socket().into_iter().collect(),
            // The helper files next to the plugin.
            read: default_helper_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned())
                .into_iter()
                .collect(),
            ..Grants::default()
        })
        .run(|configure| async move {
            let config: Config = configure.parse_config()?;
            DockerEnvironment::connect(config, configure.instance()).await
        })
        .await
}

/// The socket the plugin uses when `socket` isn't configured, which is what its sandbox grant
/// suggests. The operator sets the real grant in `[environment.<name>.sandbox]`.
fn default_socket() -> Option<String> {
    let docker_host = std::env::var("DOCKER_HOST").ok();
    Settings::resolve(Config::default(), docker_host.as_deref())
        .ok()
        .map(|settings| settings.socket().to_string_lossy().into_owned())
}
