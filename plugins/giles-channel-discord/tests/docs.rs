//! The examples in the user guide are valid for the plugin.

use giles_channel_discord::config::{Config, Settings};

const DISCORD: &str = include_str!("../../../Docs/user/discord.md");

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

/// Replaces `{ secret = ... }` and `{ env = ... }` with a plain value, as Giles does.
fn resolve(value: &mut serde_json::Value) {
    if let Some(table) = value.as_object_mut() {
        if table.len() == 1 && (table.contains_key("secret") || table.contains_key("env")) {
            *value = "resolved-token".into();
            return;
        }
        table.values_mut().for_each(resolve);
    }
}

#[test]
fn the_instances_in_the_guide_are_valid() {
    let mut configs = Vec::new();
    for document in documents(DISCORD) {
        let Some(channels) = document.get("channel").and_then(toml::Value::as_table) else {
            continue;
        };
        for (name, instance) in channels {
            let component = instance
                .get("component")
                .and_then(toml::Value::as_str)
                .unwrap_or(name);
            if component == "discord" {
                let mut config = instance.get("config").map_or_else(
                    || serde_json::json!({}),
                    |c| serde_json::to_value(c).unwrap(),
                );
                resolve(&mut config);
                configs.push(config);
            }
        }
    }
    assert_eq!(configs.len(), 2);
    for config in configs {
        let parsed: Config =
            serde_json::from_value(config.clone()).unwrap_or_else(|e| panic!("{config}: {e}"));
        if let Err(e) = Settings::new(parsed) {
            panic!("{config}: {e}");
        }
    }
}

#[test]
fn the_guide_shows_the_documented_defaults_and_limits() {
    // The settings table states these; the plugin's constants are the source.
    let config: Config = serde_json::from_value(serde_json::json!({ "token": "t" })).unwrap();
    let settings = Settings::new(config).unwrap();
    assert_eq!(settings.max_blob, 50 * 1024 * 1024);
    assert_eq!(settings.catch_up_max_age.as_secs(), 24 * 60 * 60);
    assert!(DISCORD.contains("`\"50 MiB\"`"));
    assert!(DISCORD.contains("`\"24h\"`"));

    // The longest age the guide allows is accepted, and anything above it is refused.
    for (age, ok) in [("30d", true), ("0s", true), ("720h", true), ("31d", false)] {
        let config: Config =
            serde_json::from_value(serde_json::json!({ "token": "t", "catch_up_max_age": age }))
                .unwrap();
        assert_eq!(Settings::new(config).is_ok(), ok, "{age}");
    }
    assert!(DISCORD.contains("at most `\"30d\"`"));
}

#[test]
fn the_identities_in_the_guide_have_the_form_the_plugin_reports() {
    let mut seen = 0;
    for document in documents(DISCORD) {
        let Some(identities) = document
            .get("owner")
            .and_then(|owner| owner.get("identities"))
            .and_then(toml::Value::as_array)
        else {
            continue;
        };
        for identity in identities {
            let identity = identity.as_str().unwrap();
            let native = identity
                .strip_prefix("discord:")
                .unwrap_or_else(|| panic!("{identity}: not in the discord namespace"));
            // A snowflake: decimal digits, no leading zero, and it fits 64 bits.
            assert!(!native.starts_with('0'), "{identity}");
            assert!(native.bytes().all(|b| b.is_ascii_digit()), "{identity}");
            assert!(native.parse::<u64>().is_ok(), "{identity}");
            seen += 1;
        }
    }
    assert_eq!(seen, 2);
}
