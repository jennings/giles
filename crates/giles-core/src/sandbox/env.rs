//! The command's scrubbed environment (PLAN section 12.4).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::spec::{PLUGIN_DATA_ENV, Spec};

/// The complete environment of the sandboxed command; nothing else is inherited.
///
/// - Every variable in [`Spec::env`] (validated `GILES_*` names and `LANG`).
/// - Every variable named in [`Spec::inherit_env`] that `inherited` (the trampoline's own
///   environment) has, replacing one from [`Spec::env`].
/// - `GILES_PLUGIN_DATA` and `HOME`: the data directory.
/// - `TMPDIR`: [`Spec::tmp_dir`].
/// - `PATH`: [`Spec::search_path`].
/// - `LANG`: from the above, else `inherited`'s if set.
///
/// The derived variables win over the spec's, though a valid spec can't set them.
pub fn scrubbed_env(
    spec: &Spec,
    inherited: &dyn Fn(&str) -> Option<OsString>,
) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = spec
        .env
        .iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
    for name in &spec.inherit_env {
        if let Some(value) = inherited(name) {
            env.insert(name.into(), value);
        }
    }
    if !env.contains_key(OsStr::new("LANG"))
        && let Some(lang) = inherited("LANG")
    {
        env.insert("LANG".into(), lang);
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

    /// An inherited environment of `vars`.
    fn inherited(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.into())
    }

    #[test]
    fn defaults_only() {
        let env = scrubbed_env(
            &Spec::new("/g/data/channel/irc"),
            &inherited(&[("SECRET", "x")]),
        );
        assert_eq!(
            pairs(&env),
            [
                ("GILES_PLUGIN_DATA", "/g/data/channel/irc"),
                ("HOME", "/g/data/channel/irc"),
                ("PATH", "/usr/bin:/bin"),
                ("TMPDIR", "/g/data/channel/irc/tmp"),
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
        let env = scrubbed_env(&spec, &inherited(&[("LANG", "en_US.UTF-8")]));
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
        let env = scrubbed_env(&spec, &inherited(&[("LANG", "en_US.UTF-8")]));
        assert_eq!(env[OsStr::new("LANG")], "C.UTF-8");
    }

    #[test]
    fn only_named_variables_are_inherited() {
        let mut spec = Spec::new("/d");
        spec.env.insert("GILES_X".into(), "from-spec".into());
        for name in ["GITHUB_TOKEN", "GILES_X", "UNSET"] {
            spec.inherit_env.insert(name.into());
        }
        let env = scrubbed_env(
            &spec,
            &inherited(&[
                ("GITHUB_TOKEN", "ghp_1"),
                ("GILES_X", "inherited"),
                ("OTHER", "nope"),
            ]),
        );
        assert_eq!(
            pairs(&env),
            [
                ("GILES_PLUGIN_DATA", "/d"),
                ("GILES_X", "inherited"),
                ("GITHUB_TOKEN", "ghp_1"),
                ("HOME", "/d"),
                ("PATH", "/usr/bin:/bin"),
                ("TMPDIR", "/d/tmp"),
            ]
        );
    }

    #[test]
    fn derived_variables_win_over_an_unvalidated_spec() {
        let mut spec = Spec::new("/d");
        for name in ["HOME", "TMPDIR", "PATH", PLUGIN_DATA_ENV] {
            spec.env.insert(name.into(), "/evil".into());
            spec.inherit_env.insert(name.into());
        }
        let env = scrubbed_env(&spec, &|name| (name != "LANG").then(|| "/evil".into()));
        assert!(env.values().all(|v| v != "/evil"), "{env:?}");
    }
}
