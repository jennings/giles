//! The IRC configs in the user guide are valid for the plugin.

use giles_channel_irc::{Config, Settings};

const IRC: &str = include_str!("../../../Docs/user/irc.md");
const CONFIGURATION: &str = include_str!("../../../Docs/user/configuration.md");

/// The `config` tables of the `irc` channel instances in the TOML blocks of `markdown` that parse,
/// with secret and environment references replaced by a value.
fn irc_configs(markdown: &str) -> Vec<serde_json::Value> {
    let mut configs = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line != "```toml" {
            continue;
        }
        let block: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        let Ok(document) = block.join("\n").parse::<toml::Table>() else {
            continue;
        };
        let Some(channels) = document.get("channel").and_then(toml::Value::as_table) else {
            continue;
        };
        for instance in channels.values() {
            if instance.get("component").and_then(toml::Value::as_str) == Some("irc") {
                let mut config = serde_json::to_value(&instance["config"]).unwrap();
                resolve(&mut config);
                configs.push(config);
            }
        }
    }
    configs
}

/// Replaces `{ secret = ... }` and `{ env = ... }` with a plain value, as Giles does.
fn resolve(value: &mut serde_json::Value) {
    if let Some(table) = value.as_object_mut() {
        if table.len() == 1 && (table.contains_key("secret") || table.contains_key("env")) {
            *value = "resolved-secret".into();
            return;
        }
        table.values_mut().for_each(resolve);
    }
}

fn check(configs: Vec<serde_json::Value>) {
    for config in configs {
        let parsed: Config =
            serde_json::from_value(config.clone()).unwrap_or_else(|e| panic!("{config}: {e}"));
        if let Err(e) = Settings::new(parsed) {
            panic!("{config}: {e}");
        }
    }
}

#[test]
fn the_irc_page_examples_are_valid() {
    let configs = irc_configs(IRC);
    assert_eq!(configs.len(), 1);
    check(configs);
}

#[test]
fn the_configuration_page_examples_are_valid() {
    let configs = irc_configs(CONFIGURATION);
    assert_eq!(configs.len(), 3);
    check(configs);
}
