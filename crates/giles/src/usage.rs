//! `giles usage`: token usage per agent and model, with the prompt cache hit rate (PLAN section
//! 11.7), from the admin service's `UsageReport`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use giles_plugin_sdk::config::parse_duration;
use giles_plugin_sdk::convert::timestamp_to_proto;
use giles_proto::admin::v1 as pb;
use giles_types::Usage;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::admin_client::{self, local_time};
use crate::cli::UsageArgs;

/// Prints the usage tables.
pub fn run(home: Option<&Path>, args: &UsageArgs) -> anyhow::Result<()> {
    let tz = TimeZone::system();
    let since = args
        .since
        .as_deref()
        .map(|text| parse_since(text, Timestamp::now(), &tz))
        .transpose()?;
    let rows = admin_client::call(home, async |mut client| {
        let request = pb::UsageReportRequest {
            since: since.map(timestamp_to_proto),
            until: None,
        };
        Ok(client.usage_report(request).await?.into_inner().rows)
    })?;
    let since = since.map(|at| local_time(at, &tz));
    print!("{}", format_usage(&rows, since.as_deref()));
    Ok(())
}

/// `--since`: an RFC 3339 time, a date (from midnight in `tz`), or a duration before `now`.
fn parse_since(text: &str, now: Timestamp, tz: &TimeZone) -> anyhow::Result<Timestamp> {
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    if let Ok(date) = text.parse::<Date>() {
        return Ok(date
            .to_zoned(tz.clone())
            .with_context(|| format!("--since {text}"))?
            .timestamp());
    }
    if let Ok(ago) = parse_duration(text) {
        return SignedDuration::try_from(ago)
            .ok()
            .and_then(|ago| now.checked_sub(ago).ok())
            .with_context(|| format!("--since {text}: too far back"));
    }
    bail!(
        "invalid --since {text:?}: expected a duration ago (\"7d\", \"12h\"), a date \
         (\"2026-10-01\"), or an RFC 3339 time (\"2026-10-01T09:00:00Z\")"
    )
}

/// Totals for one table row.
#[derive(Default)]
struct Totals {
    requests: u64,
    fallbacks: u64,
    tokens: Usage,
}

impl Totals {
    fn add(&mut self, row: &pb::UsageRow) {
        self.requests += row.requests;
        self.fallbacks += row.fallback_requests;
        if let Some(u) = &row.usage {
            self.tokens.input_tokens += u.input_tokens;
            self.tokens.output_tokens += u.output_tokens;
            self.tokens.cache_read_tokens += u.cache_read_tokens;
            self.tokens.cache_write_tokens += u.cache_write_tokens;
        }
    }

    /// The numeric columns.
    fn cells(&self) -> Vec<String> {
        let t = &self.tokens;
        let hit_rate = t
            .cache_hit_rate()
            .map_or_else(|| "-".to_owned(), |rate| format!("{:.1}%", rate * 100.0));
        vec![
            self.requests.to_string(),
            self.fallbacks.to_string(),
            t.input_tokens.to_string(),
            t.output_tokens.to_string(),
            t.cache_read_tokens.to_string(),
            t.cache_write_tokens.to_string(),
            hit_rate,
        ]
    }
}

const NUMBER_HEADERS: [&str; 7] = [
    "REQUESTS",
    "FALLBACKS",
    "INPUT",
    "OUTPUT",
    "CACHE READ",
    "CACHE WRITE",
    "CACHE HIT",
];

/// One table per agent, then per agent, provider instance, and model. `since` is the start of
/// the period, for the header.
fn format_usage(rows: &[pb::UsageRow], since: Option<&str>) -> String {
    let period = since.map_or_else(|| " (all time)".to_owned(), |at| format!(" since {at}"));
    if rows.is_empty() {
        return format!("No provider requests{period}.\n");
    }
    let mut agents: BTreeMap<&str, Totals> = BTreeMap::new();
    for row in rows {
        agents.entry(&row.agent).or_default().add(row);
    }
    let per_agent = agents
        .iter()
        .map(|(agent, totals)| [vec![(*agent).to_owned()], totals.cells()].concat())
        .collect();
    let per_model = rows
        .iter()
        .map(|row| {
            let mut totals = Totals::default();
            totals.add(row);
            [
                vec![row.agent.clone(), row.provider.clone(), row.model.clone()],
                totals.cells(),
            ]
            .concat()
        })
        .collect();
    format!(
        "Usage per agent{period}:\n\n{}\nPer model:\n\n{}",
        table(&["AGENT"], per_agent),
        table(&["AGENT", "PROVIDER", "MODEL"], per_model),
    )
}

/// A plain-text table: the `text` columns left-aligned, then [`NUMBER_HEADERS`] right-aligned.
fn table(text: &[&str], rows: Vec<Vec<String>>) -> String {
    let header: Vec<String> = text
        .iter()
        .chain(&NUMBER_HEADERS)
        .map(|h| (*h).to_owned())
        .collect();
    let all: Vec<&Vec<String>> = std::iter::once(&header).chain(&rows).collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| all.iter().map(|row| row[i].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in all {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (cell, &width))| {
                if i < text.len() {
                    format!("{cell:<width$}")
                } else {
                    format!("{cell:>width$}")
                }
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use giles_proto::types::v1 as types_pb;

    use super::*;

    fn row(agent: &str, model: &str, requests: u64, usage: [u64; 4]) -> pb::UsageRow {
        pb::UsageRow {
            agent: agent.into(),
            provider: "anthropic".into(),
            model: model.into(),
            requests,
            fallback_requests: u64::from(model == "haiku"),
            usage: Some(types_pb::Usage {
                input_tokens: usage[0],
                output_tokens: usage[1],
                cache_read_tokens: usage[2],
                cache_write_tokens: usage[3],
            }),
        }
    }

    #[test]
    fn usage_tables_sum_per_agent_with_the_cache_hit_rate() {
        let rows = [
            row("bunter", "claude", 1, [10, 5, 0, 0]),
            row("jeeves", "claude", 2, [100, 50, 800, 100]),
            row("jeeves", "haiku", 1, [100, 10, 0, 0]),
        ];
        let expected = "\
Usage per agent since 2026-10-01 00:00:00 UTC:

AGENT   REQUESTS  FALLBACKS  INPUT  OUTPUT  CACHE READ  CACHE WRITE  CACHE HIT
bunter         1          0     10       5           0            0       0.0%
jeeves         3          1    200      60         800          100      72.7%

Per model:

AGENT   PROVIDER   MODEL   REQUESTS  FALLBACKS  INPUT  OUTPUT  CACHE READ  CACHE WRITE  CACHE HIT
bunter  anthropic  claude         1          0     10       5           0            0       0.0%
jeeves  anthropic  claude         2          0    100      50         800          100      80.0%
jeeves  anthropic  haiku          1          1    100      10           0            0       0.0%
";
        assert_eq!(
            format_usage(&rows, Some("2026-10-01 00:00:00 UTC")),
            expected
        );
    }

    #[test]
    fn no_requests_and_no_input_tokens() {
        assert_eq!(
            format_usage(&[], None),
            "No provider requests (all time).\n"
        );
        let text = format_usage(&[row("a", "m", 1, [0, 3, 0, 0])], None);
        assert!(text.starts_with("Usage per agent (all time):"), "{text}");
        assert!(text.lines().nth(3).unwrap().ends_with("  -"), "{text}");
    }

    #[test]
    fn since_takes_a_time_a_date_or_a_duration_ago() {
        let now: Timestamp = "2026-10-23T12:00:00Z".parse().unwrap();
        let tz = TimeZone::fixed(jiff::tz::offset(-7));
        let since = |text| parse_since(text, now, &tz).map(|t| t.to_string());
        assert_eq!(
            since("2026-10-01T09:00:00Z").unwrap(),
            "2026-10-01T09:00:00Z"
        );
        assert_eq!(since("2026-10-01").unwrap(), "2026-10-01T07:00:00Z");
        assert_eq!(since("7d").unwrap(), "2026-10-16T12:00:00Z");
        assert_eq!(since("1h30m").unwrap(), "2026-10-23T10:30:00Z");
        let err = since("last week").unwrap_err().to_string();
        assert!(err.contains("invalid --since"), "{err}");
    }
}
