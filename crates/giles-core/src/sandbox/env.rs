//! The command's scrubbed environment (PLAN section 12.4).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::spec::{PLUGIN_DATA_ENV, Spec};

/// The complete environment of the sandboxed command; nothing else is inherited.
///
/// - Every variable in [`Spec::env`] (validated `GILES_*` names and `LANG`).
/// - `GILES_PLUGIN_DATA` and `HOME`: the data directory.
/// - `TMPDIR`: [`Spec::tmp_dir`].
/// - `PATH`: [`Spec::search_path`].
/// - `LANG`: from the spec, else `inherited_lang` (the trampoline's own `LANG`) if set.
///
/// The derived variables win over the spec's, though a valid spec can't set them.
pub fn scrubbed_env(spec: &Spec, inherited_lang: Option<&OsStr>) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = spec
        .env
        .iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
    if let Some(lang) = inherited_lang {
        env.entry("LANG".into()).or_insert_with(|| lang.to_owned());
    }
    env.insert(PLUGIN_DATA_ENV.into(), spec.data_dir.clone().into());
    env.insert("HOME".into(), spec.data_dir.clone().into());
    env.insert("TMPDIR".into(), spec.tmp_dir().into());
    env.insert("PATH".into(), spec.search_path().into());
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(env: &BTreeMap<OsString, OsString>) -> Vec<(&str, &str)> {
        env.iter()
            .map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap()))
            .collect()
    }

    #[test]
    fn defaults_only() {
        let env = scrubbed_env(&Spec::new("/g/data/plugins/irc"), None);
        assert_eq!(
            pairs(&env),
            [
                ("GILES_PLUGIN_DATA", "/g/data/plugins/irc"),
                ("HOME", "/g/data/plugins/irc"),
                ("PATH", "/usr/bin:/bin"),
                ("TMPDIR", "/g/data/plugins/irc/tmp"),
            ]
        );
    }

    #[test]
    fn spec_variables_pass_and_lang_is_inherited() {
        let mut spec = Spec::new("/d");
        spec.env
            .insert("GILES_PLUGIN_MAGIC".into(), "cookie".into());
        spec.env.insert("GILES_PLUGIN_PROTOCOLS".into(), "1".into());
        spec.path = Some("/opt/bin:/usr/bin".into());
        let env = scrubbed_env(&spec, Some(OsStr::new("en_US.UTF-8")));
        assert_eq!(
            pairs(&env),
            [
                ("GILES_PLUGIN_DATA", "/d"),
                ("GILES_PLUGIN_MAGIC", "cookie"),
                ("GILES_PLUGIN_PROTOCOLS", "1"),
                ("HOME", "/d"),
                ("LANG", "en_US.UTF-8"),
                ("PATH", "/opt/bin:/usr/bin"),
                ("TMPDIR", "/d/tmp"),
            ]
        );
    }

    #[test]
    fn spec_lang_beats_inherited_lang() {
        let mut spec = Spec::new("/d");
        spec.env.insert("LANG".into(), "C.UTF-8".into());
        let env = scrubbed_env(&spec, Some(OsStr::new("en_US.UTF-8")));
        assert_eq!(env[OsStr::new("LANG")], "C.UTF-8");
    }

    #[test]
    fn derived_variables_win_over_an_unvalidated_spec() {
        let mut spec = Spec::new("/d");
        for name in ["HOME", "TMPDIR", "PATH", PLUGIN_DATA_ENV] {
            spec.env.insert(name.into(), "/evil".into());
        }
        let env = scrubbed_env(&spec, None);
        assert!(env.values().all(|v| v != "/evil"), "{env:?}");
    }
}
