//! The examples in the user guide are valid for the plugin.

use giles_environment_docker::{Config, Settings};

const GUIDE: &str = include_str!("../../../Docs/user/docker-environment.md");

/// The TOML documents in the ```` ```toml ```` blocks of `markdown` that parse.
fn documents(markdown: &str) -> Vec<toml::Table> {
    let mut documents = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line != "```toml" {
            continue;
        }
        let block: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        if let Ok(document) = block.join("\n").parse::<toml::Table>() {
            documents.push(document);
        }
    }
    documents
}

/// Replaces `{ secret = ... }` with a plain value, as Giles does.
fn resolve(value: &mut serde_json::Value) {
    if let Some(table) = value.as_object_mut() {
        if table.len() == 1 && table.contains_key("secret") {
            *value = "resolved".into();
            return;
        }
        table.values_mut().for_each(resolve);
    }
}

fn check(config: serde_json::Value) {
    let mut config = config;
    resolve(&mut config);
    let parsed = Config::from_json(config.clone()).unwrap_or_else(|e| panic!("{config}: {e}"));
    if let Err(e) = Settings::resolve(parsed, None) {
        panic!("{config}: {e}");
    }
}

#[test]
fn the_plugin_settings_in_the_guide_are_valid() {
    let mut checked = 0;
    for document in documents(GUIDE) {
        // A launched instance: `[environment.<name>.config]`.
        if let Some(environments) = document.get("environment").and_then(toml::Value::as_table) {
            for instance in environments.values() {
                if let Some(config) = instance.get("config") {
                    check(serde_json::to_value(config).unwrap());
                    checked += 1;
                }
            }
        }
        // A remote plugin's own file: its `[config]` table.
        if let Some(config) = document.get("config") {
            check(serde_json::to_value(config).unwrap());
            checked += 1;
        }
    }
    // The launched example, the remote file, the allowlist, the registry login, the limits, and env.
    assert!(checked >= 6, "{checked} settings tables checked");
}

#[test]
fn the_guide_states_the_defaults_and_limits() {
    let settings = Settings::resolve(Config::default(), None).unwrap();
    assert_eq!(
        settings.allowlist().describe(),
        "ubuntu:*, debian:*, alpine:*, python:*, node:*, golang:*, rust:*, ruby:*, eclipse-temurin:*"
    );
    for pattern in [
        "`ubuntu:*`",
        "`debian:*`",
        "`alpine:*`",
        "`python:*`",
        "`node:*`",
        "`golang:*`",
        "`rust:*`",
        "`ruby:*`",
        "`eclipse-temurin:*`",
    ] {
        assert!(GUIDE.contains(pattern), "{pattern}");
    }
    assert_eq!(settings.socket().to_str(), Some("/var/run/docker.sock"));
    assert!(GUIDE.contains("`/var/run/docker.sock`"));
    assert!(GUIDE.contains("`\"bridge\"`"));
}
