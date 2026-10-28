//! Remote toolsets for a test configuration: they reach the running configuration as stored
//! toolsets do (PLAN section 7.10), from specs written as TOML. `giles.toml` ignores a
//! `[toolset.<name>]` table with `url`.
#![allow(dead_code)] // Each test file uses a part of it.

use giles_core::config::{
    Config, LoadOptions, Secrets, ToolsetContext, ToolsetSpec, validate_toolset,
};
use giles_core::paths::DataRoot;

/// Adds each `(name, spec)` to `config` as a toolset, in order. `spec` is TOML text with the keys
/// of a toolset (`url = "https://example.com/mcp"`), at the top level. Each toolset is validated
/// against `root`, its `secrets.toml` as it is now, no environment, and the toolsets added before
/// it.
///
/// # Panics
///
/// If a spec doesn't parse or validate.
pub fn add_toolsets(root: &DataRoot, config: &mut Config, toolsets: &[(&str, &str)]) {
    let secrets =
        Secrets::load(&root.secrets_file()).unwrap_or_else(|error| panic!("secrets: {error}"));
    let options = LoadOptions {
        env: &|_| None,
        giles_exe: None,
        ..LoadOptions::default()
    };
    for (name, spec) in toolsets {
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
