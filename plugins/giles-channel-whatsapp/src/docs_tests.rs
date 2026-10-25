//! The examples in the user guide are valid for the plugin.

use crate::config::{Config, Settings};
use crate::identity::parse_phone;

const WHATSAPP: &str = include_str!("../../../Docs/user/whatsapp.md");

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

#[test]
fn the_instances_in_the_guide_are_valid() {
    let mut configs = Vec::new();
    for document in documents(WHATSAPP) {
        let Some(channels) = document.get("channel").and_then(toml::Value::as_table) else {
            continue;
        };
        for (name, instance) in channels {
            let component = instance
                .get("component")
                .and_then(toml::Value::as_str)
                .unwrap_or(name);
            if component == "whatsapp" {
                configs.push(instance.get("config").map_or_else(
                    || serde_json::json!({}),
                    |c| serde_json::to_value(c).unwrap(),
                ));
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
fn the_identities_in_the_guide_have_the_form_the_plugin_reports() {
    let mut seen = 0;
    for document in documents(WHATSAPP) {
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
                .strip_prefix("whatsapp:")
                .unwrap_or_else(|| panic!("{identity}: not in the whatsapp namespace"));
            // The canonical form of a phone number, or a LID.
            match native.strip_suffix("@lid") {
                Some(user) => assert!(user.bytes().all(|b| b.is_ascii_digit()), "{identity}"),
                None => assert_eq!(
                    parse_phone(native)
                        .map(|digits| format!("+{digits}"))
                        .as_deref(),
                    Ok(native),
                    "{identity}"
                ),
            }
            seen += 1;
        }
    }
    assert_eq!(seen, 3);
}
