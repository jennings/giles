//! The plugin keeps its own license: it links AGPL-3.0-only code (`wacore-libsignal`), so it must
//! not inherit the workspace's `MIT OR Apache-2.0`, and the text and notices must ship with it.

use std::path::Path;

fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_crate_is_agpl_not_the_workspace_license() {
    let manifest: toml::Table = read("Cargo.toml").parse().unwrap();
    let license = manifest["package"]["license"].as_str();
    assert_eq!(
        license,
        Some("AGPL-3.0-only"),
        "the package must set its own license (not `license.workspace = true`)"
    );
}

#[test]
fn the_license_text_and_notice_are_in_the_crate() {
    assert!(read("LICENSE-AGPL-3.0").contains("GNU AFFERO GENERAL PUBLIC LICENSE"));
    // The source offer names the pinned library commit, which must be the one in Cargo.toml.
    let manifest = read("Cargo.toml");
    let rev = manifest
        .lines()
        .find_map(|line| line.split("rev = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("the whatsapp-rust rev in Cargo.toml");
    let notice = read("THIRD-PARTY");
    assert!(notice.contains(rev), "THIRD-PARTY names rev {rev}");
    assert!(notice.contains("wacore-libsignal"));
}

/// The AGPL stays in this crate: no other crate may depend on the plugin or on the library with
/// the AGPL code, and the workspace's shared dependency table mustn't offer it.
#[test]
fn no_other_crate_depends_on_the_plugin_or_its_agpl_library() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut manifests = vec![root.join("Cargo.toml")];
    for dir in ["crates", "plugins"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let manifest = entry.unwrap().path().join("Cargo.toml");
            if manifest.exists() {
                manifests.push(manifest);
            }
        }
    }
    assert!(manifests.len() > 5, "found the workspace manifests");
    for manifest in manifests {
        if manifest.canonicalize().unwrap()
            == Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("Cargo.toml")
                .canonicalize()
                .unwrap()
        {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).unwrap();
        for forbidden in ["giles-channel-whatsapp", "whatsapp-rust", "wacore"] {
            assert!(
                !text.contains(forbidden),
                "{} mentions {forbidden}: the AGPL code must stay in plugins/giles-channel-whatsapp",
                manifest.display()
            );
        }
    }
}
