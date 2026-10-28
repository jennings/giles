//! `giles toolset`: list, show, set, remove, and restart the MCP toolsets of the running daemon,
//! over the admin service's `ListToolsets`, `GetToolset`, `SetToolset`, `RemoveToolset`, and
//! `RestartPlugin` (PLAN section 7.10).
//!
//! `show` prints a spec as TOML with an `# updated_at = <time>` comment, and `set` sends that time
//! back as `expected_updated_at`, so `show`, an edit, and `set` can't overwrite a change made
//! meanwhile.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use giles_plugin_sdk::convert::{timestamp_from_proto, timestamp_to_proto};
use giles_proto::admin::v1 as pb;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use tonic::{Code, Status};

use crate::admin_client::{self, proto_time};
use crate::cli::{ToolsetArgs, ToolsetCommand};

/// The comment key that carries the time of the last change.
const UPDATED_AT: &str = "updated_at";

/// Runs one `giles toolset` command.
pub fn run(home: Option<&Path>, args: &ToolsetArgs) -> anyhow::Result<()> {
    match &args.command {
        ToolsetCommand::List => list(home),
        ToolsetCommand::Show(args) => show(home, &args.name),
        ToolsetCommand::Set(args) => set(home, &args.name, &args.file),
        ToolsetCommand::Remove(args) => remove(home, &args.name),
        ToolsetCommand::Restart(args) => restart(home, &args.name),
    }
}

fn list(home: Option<&Path>) -> anyhow::Result<()> {
    let toolsets = admin_client::call(home, async |mut client| {
        Ok(client
            .list_toolsets(pb::ListToolsetsRequest {})
            .await?
            .into_inner()
            .toolsets)
    })?;
    print!("{}", format_list(&toolsets, &TimeZone::system()));
    Ok(())
}

/// Prints the spec on standard output (alone, so it can be redirected to a file) and a toolset
/// that isn't running on standard error.
fn show(home: Option<&Path>, name: &str) -> anyhow::Result<()> {
    let request = pb::GetToolsetRequest {
        name: name.to_owned(),
    };
    let toolset = admin_client::call(home, async |mut client| {
        Ok(client.get_toolset(request).await?.into_inner().toolset)
    })?
    .context("the daemon sent no toolset")?;
    let text = format_show(&toolset)?;
    if let Some(failure) = &toolset.failure {
        eprintln!("toolset `{}` isn't running: {failure}", toolset.name);
    }
    print!("{text}");
    Ok(())
}

fn set(home: Option<&Path>, name: &str, file: &Path) -> anyhow::Result<()> {
    let spec_toml = read_spec(file)?;
    let expected = read_updated_at(&spec_toml)?;
    let request = pb::SetToolsetRequest {
        name: name.to_owned(),
        spec_toml,
        expected_updated_at: expected.map(timestamp_to_proto),
    };
    let saved = admin_client::call(home, async |mut client| {
        client
            .set_toolset(request)
            .await
            .map_err(|status| with_hint(status, expected.is_some()))
    })?;
    // The name as the toolset has it: a replaced toolset keeps its spelling.
    let name = saved
        .into_inner()
        .toolset
        .map_or_else(|| name.to_owned(), |toolset| toolset.name);
    println!("Saved toolset `{name}`.");
    Ok(())
}

fn remove(home: Option<&Path>, name: &str) -> anyhow::Result<()> {
    let request = pb::RemoveToolsetRequest {
        name: name.to_owned(),
    };
    let removed = admin_client::call(home, async |mut client| {
        Ok(client.remove_toolset(request).await?.into_inner())
    })?;
    println!("Removed toolset `{}`.", removed.name);
    Ok(())
}

/// Restarts the toolset through `RestartPlugin`, under the name `GetToolset` gives it (the name
/// matches ignoring case, but a restart needs it as it was written), so only a toolset is
/// restarted and not a channel that happens to have the name.
fn restart(home: Option<&Path>, name: &str) -> anyhow::Result<()> {
    let request = pb::GetToolsetRequest {
        name: name.to_owned(),
    };
    let toolset = admin_client::call(home, async |mut client| {
        let toolset = client
            .get_toolset(request)
            .await?
            .into_inner()
            .toolset
            .ok_or_else(|| Status::internal("the daemon sent no toolset"))?;
        client
            .restart_plugin(pb::RestartPluginRequest {
                instance: toolset.name.clone(),
            })
            .await?;
        Ok(toolset)
    })?;
    println!("Restarting toolset `{}`.", toolset.name);
    Ok(())
}

/// The spec text: the file, or standard input for `-`.
fn read_spec(file: &Path) -> anyhow::Result<String> {
    if file == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read the spec from standard input")?;
        return Ok(text);
    }
    std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))
}

/// What a failed `set` should say to a caller that sent an `updated_at`: there is no toolset with
/// that name, which also happens when the file is a copy of another toolset's `show` output.
fn with_hint(status: Status, sent_updated_at: bool) -> Status {
    if sent_updated_at && status.code() == Code::NotFound {
        return Status::not_found(format!(
            "{}; to create it, remove the `# {UPDATED_AT}` comment from the spec",
            status.message()
        ));
    }
    status
}

/// The spec as TOML, after the `# updated_at` comment for `set` to send back. A toolset of
/// `giles.toml` has no stored spec.
fn format_show(toolset: &pb::Toolset) -> anyhow::Result<String> {
    if toolset.source() == pb::ToolsetSource::GilesToml {
        bail!(
            "toolset `{}` is configured in giles.toml, which has the spec: edit it there",
            toolset.name
        );
    }
    let Some(spec) = &toolset.spec_toml else {
        bail!(
            "the stored spec of toolset `{}` can't be shown: {}",
            toolset.name,
            toolset.failure.as_deref().unwrap_or("unknown reason")
        );
    };
    let mut out = String::new();
    if let Some(at) = toolset.updated_at {
        let at = timestamp_from_proto(at)
            .map_err(|err| anyhow!("the daemon sent an invalid updated_at: {err}"))?;
        let _ = writeln!(out, "# {UPDATED_AT} = {at}");
    }
    out.push_str(spec);
    Ok(out)
}

/// The time in the `# updated_at = <time>` comment that `show` writes, if the text has one.
fn read_updated_at(text: &str) -> anyhow::Result<Option<Timestamp>> {
    for line in text.lines() {
        let Some(comment) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let Some(rest) = comment.trim_start().strip_prefix(UPDATED_AT) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        return value.parse().map(Some).with_context(|| {
            format!(
                "the `# {UPDATED_AT}` comment says {value:?}, which isn't a time like \
                 2026-10-27T14:03:12Z"
            )
        });
    }
    Ok(None)
}

/// A table of the toolsets: name, where it is configured, when it changed (in `tz`), and the
/// reason it isn't running, if it isn't.
fn format_list(toolsets: &[pb::Toolset], tz: &TimeZone) -> String {
    if toolsets.is_empty() {
        return "No toolsets.\n".to_owned();
    }
    let header = ["NAME", "SOURCE", "UPDATED", "PROBLEM"].map(str::to_owned);
    let rows: Vec<[String; 4]> = toolsets
        .iter()
        .map(|toolset| {
            let source = match toolset.source() {
                pb::ToolsetSource::Stored => "stored",
                pb::ToolsetSource::GilesToml => "giles.toml",
                pb::ToolsetSource::Unspecified => "unknown",
            };
            [
                toolset.name.clone(),
                source.to_owned(),
                toolset
                    .updated_at
                    .map_or_else(|| "-".to_owned(), |at| proto_time(at, tz)),
                toolset
                    .failure
                    .as_deref()
                    .map(|reason| reason.replace('\n', " "))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    let widths: [usize; 3] = std::array::from_fn(|column| {
        std::iter::once(&header)
            .chain(&rows)
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
    });
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let line = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use giles_core::config::ToolsetSpec;

    use super::*;

    fn toolset(name: &str, spec: Option<&str>, updated: Option<i64>) -> pb::Toolset {
        pb::Toolset {
            name: name.into(),
            source: if updated.is_some() {
                pb::ToolsetSource::Stored
            } else {
                pb::ToolsetSource::GilesToml
            }
            .into(),
            spec_toml: spec.map(str::to_owned),
            updated_at: updated.map(|secs| prost_types::Timestamp {
                seconds: 1_800_000_000 + secs,
                nanos: 250_000_000,
            }),
            failure: None,
        }
    }

    const SPEC: &str =
        "url = \"https://files.example.com/mcp\"\n\n[auth]\nbearer = { secret = \"files\" }\n";

    #[test]
    fn show_prints_the_spec_after_a_comment_that_set_reads_back() {
        let shown = format_show(&toolset("files", Some(SPEC), Some(5))).unwrap();
        assert_eq!(
            shown,
            format!("# updated_at = 2027-01-15T08:00:05.25Z\n{SPEC}")
        );

        // `set` reads the time back, exactly, and the daemon reads the text as the spec.
        let expected = read_updated_at(&shown).unwrap().unwrap();
        assert_eq!(
            timestamp_to_proto(expected),
            prost_types::Timestamp {
                seconds: 1_800_000_005,
                nanos: 250_000_000,
            }
        );
        assert_eq!(
            ToolsetSpec::from_toml(&shown).unwrap(),
            ToolsetSpec::from_toml(SPEC).unwrap()
        );
    }

    #[test]
    fn a_toolset_without_a_spec_is_not_shown() {
        let err = format_show(&toolset("github", None, None)).unwrap_err();
        assert!(err.to_string().contains("giles.toml"), "{err}");

        let mut damaged = toolset("files", None, Some(1));
        damaged.failure = Some("the stored spec can't be read: url: invalid type".into());
        let err = format_show(&damaged).unwrap_err().to_string();
        assert!(err.contains("can't be read: url"), "{err}");
    }

    #[test]
    fn the_updated_at_comment_is_found_anywhere_and_must_be_a_time() {
        assert_eq!(read_updated_at(SPEC).unwrap(), None);
        // Other comments, and a key that only starts like it, are not it.
        assert_eq!(
            read_updated_at("# the time it was updated_at\n#updated_at_x = 5\nupdated_at = 5\n")
                .unwrap(),
            None
        );
        for text in [
            "#updated_at=2026-10-27T14:03:12Z\nurl = \"https://x.example.com/mcp\"\n",
            "url = \"https://x.example.com/mcp\"\n   #   updated_at   =   2026-10-27T14:03:12Z  \n",
        ] {
            assert_eq!(
                read_updated_at(text).unwrap().unwrap().to_string(),
                "2026-10-27T14:03:12Z",
                "{text}"
            );
        }
        let err = read_updated_at("# updated_at = yesterday\n").unwrap_err();
        assert!(err.to_string().contains("\"yesterday\""), "{err}");
    }

    #[test]
    fn a_missing_toolset_with_an_updated_at_says_how_to_create_it() {
        let status = with_hint(Status::not_found("there is no toolset called `b`"), true);
        assert_eq!(status.code(), Code::NotFound);
        assert!(
            status.message().starts_with(
                "there is no toolset called `b`; to create it, remove the `# updated_at`"
            ),
            "{}",
            status.message()
        );
        // Without one, and for any other status, it is as the daemon said.
        let plain = with_hint(Status::not_found("there is no toolset called `b`"), false);
        assert_eq!(plain.message(), "there is no toolset called `b`");
        let other = with_hint(Status::failed_precondition("changed"), true);
        assert_eq!(other.message(), "changed");
    }

    #[test]
    fn the_list_is_a_table_with_the_problems_last() {
        let mut broken = toolset("notes", Some(SPEC), Some(7));
        broken.failure = Some("auth.bearer: no secret \"notes\"\nin secrets.toml".into());
        let toolsets = [
            toolset("files", Some(SPEC), Some(5)),
            toolset("github", None, None),
            broken,
        ];
        let text = format_list(&toolsets, &TimeZone::UTC);
        assert_eq!(
            text,
            "\
NAME    SOURCE      UPDATED                  PROBLEM
files   stored      2027-01-15 08:00:05 UTC
github  giles.toml  -
notes   stored      2027-01-15 08:00:07 UTC  auth.bearer: no secret \"notes\" in secrets.toml
"
        );
        assert_eq!(format_list(&[], &TimeZone::UTC), "No toolsets.\n");
    }
}
