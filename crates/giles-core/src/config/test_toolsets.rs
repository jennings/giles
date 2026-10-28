//! Test setup: remote toolsets added to a loaded [`Config`], as stored toolsets reach the running
//! configuration (PLAN section 7.10). `giles.toml` ignores a `[toolset.<name>]` table with `url`.

use super::{Config, LoadOptions, Secrets, ToolsetContext, ToolsetSpec, validate_toolset};
use crate::paths::DataRoot;

/// Adds each `(name, spec)` to `config` as a toolset, in order. `spec` is TOML text with the keys
/// of a toolset (`url = "https://example.com/mcp"`), at the top level, as
/// [`ToolsetSpec::from_toml`] reads it. Each toolset is validated against `root`, its
/// `secrets.toml` as it is now, no environment, and the toolsets added before it.
///
/// # Panics
///
/// If a spec doesn't parse or validate.
pub(crate) fn add_toolsets<N: AsRef<str>, S: AsRef<str>>(
    root: &DataRoot,
    config: &mut Config,
    toolsets: impl IntoIterator<Item = (N, S)>,
) {
    let secrets =
        Secrets::load(&root.secrets_file()).unwrap_or_else(|error| panic!("secrets: {error}"));
    let options = LoadOptions {
        env: &|_| None,
        giles_exe: None,
        ..LoadOptions::default()
    };
    for (name, spec) in toolsets {
        let (name, spec) = (name.as_ref(), spec.as_ref());
        let parsed =
            ToolsetSpec::from_toml(spec).unwrap_or_else(|error| panic!("toolset {name}: {error}"));
        let instance = validate_toolset(
            name,
            &parsed,
            &ToolsetContext {
                root,
                secrets: &secrets,
                options: &options,
                config,
                replacing: None,
            },
        )
        .unwrap_or_else(|error| panic!("toolset {name}: {error}"));
        config.toolsets.insert(instance.name.clone(), instance);
    }
}
