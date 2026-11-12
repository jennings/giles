//! Conversions between `giles-types`' environment types and the `giles.environment.v1` wire types,
//! by moving fields, following the mapping table in `Docs/technical/protocols.md`.
//!
//! The same rules as the rest of [`convert`](super): wire to domain validates (ids are checked,
//! unspecified enum values and unset `oneof`s are refused, as [`Error::InvalidRequest`] or
//! [`Error::InvalidId`]), and domain to wire fails only for a variant of a `#[non_exhaustive]`
//! domain enum this version doesn't know. The SDK uses the plugin's direction of each (requests
//! from the wire, responses to it); the other is public for the core's `RemoteEnvironment`
//! adapter and for tests.
//!
//! A path or directory the domain types leave out (`None`) is the empty string on the wire.

use std::time::Duration;

use giles_proto::environment::v1 as pb;
use giles_types::{
    EnvironmentInfo, ExecEvent, ExecExit, ExecRequest, FindRequest, FindResult, GrepMatch,
    GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Workspace, WorkspaceDetail,
    WorkspaceId, WorkspaceList, WorkspaceState,
};

use super::{invalid, timestamp_from_proto, timestamp_to_proto};

/// `Duration` to `google.protobuf.Duration`. A duration beyond the wire type's range (more than
/// about 292 billion years) is clamped to its largest.
pub fn duration_to_proto(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(duration.subsec_nanos()).expect("less than 1e9"),
    }
}

/// `google.protobuf.Duration` to `Duration`. A negative duration is invalid.
pub fn duration_from_proto(duration: prost_types::Duration) -> Result<Duration> {
    let seconds = u64::try_from(duration.seconds).ok();
    let nanos = u32::try_from(duration.nanos)
        .ok()
        .filter(|n| *n < 1_000_000_000);
    match (seconds, nanos) {
        (Some(seconds), Some(nanos)) => Ok(Duration::new(seconds, nanos)),
        _ => Err(invalid(format!(
            "duration {}s {}ns is negative or malformed",
            duration.seconds, duration.nanos
        ))),
    }
}

/// `EnvironmentInfo` to an `InfoResponse`.
pub fn info_to_proto(info: &EnvironmentInfo) -> pb::InfoResponse {
    pb::InfoResponse {
        options_schema_json: info.options_schema.to_string(),
    }
}

/// An `InfoResponse` to `EnvironmentInfo`. The schema must be JSON.
pub fn info_from_proto(info: pb::InfoResponse) -> Result<EnvironmentInfo> {
    let options_schema = serde_json::from_str(&info.options_schema_json)
        .map_err(|e| invalid(format!("options_schema_json is not JSON: {e}")))?;
    Ok(EnvironmentInfo { options_schema })
}

/// The options of a `CreateWorkspaceRequest`: a JSON object. An empty string is `{}`.
pub fn options_from_proto(options_json: &str) -> Result<serde_json::Value> {
    if options_json.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str(options_json) {
        Ok(options @ serde_json::Value::Object(_)) => Ok(options),
        Ok(_) => Err(invalid("options_json must be a JSON object")),
        Err(e) => Err(invalid(format!("options_json is not JSON: {e}"))),
    }
}

/// `Workspace` to its wire form.
pub fn workspace_to_proto(workspace: Workspace) -> Result<pb::Workspace> {
    let state = match workspace.state {
        WorkspaceState::Running => pb::WorkspaceState::Running,
        WorkspaceState::Stopped => pb::WorkspaceState::Stopped,
        other => {
            return Err(invalid(format!(
                "workspace state {other:?} has no wire form"
            )));
        }
    };
    Ok(pb::Workspace {
        workspace_id: workspace.id.into_string(),
        description: workspace.description,
        working_directory: workspace.working_directory,
        persistence: workspace.persistence,
        created_at: Some(timestamp_to_proto(workspace.created_at)),
        state: state.into(),
        disk_used_bytes: workspace.disk_used_bytes,
        details: workspace
            .details
            .into_iter()
            .map(|d| pb::WorkspaceDetail {
                label: d.label,
                value: d.value,
            })
            .collect(),
    })
}

/// A wire `Workspace` to `Workspace`. The id, the creation time, and the state are required.
pub fn workspace_from_proto(workspace: pb::Workspace) -> Result<Workspace> {
    let state = match workspace.state() {
        pb::WorkspaceState::Running => WorkspaceState::Running,
        pb::WorkspaceState::Stopped => WorkspaceState::Stopped,
        pb::WorkspaceState::Unspecified => return Err(invalid("workspace state: unspecified")),
    };
    Ok(Workspace {
        id: WorkspaceId::new(workspace.workspace_id)?,
        description: workspace.description,
        working_directory: workspace.working_directory,
        persistence: workspace.persistence,
        created_at: timestamp_from_proto(
            workspace
                .created_at
                .ok_or_else(|| invalid("workspace created_at: missing"))?,
        )?,
        state,
        disk_used_bytes: workspace.disk_used_bytes,
        details: workspace
            .details
            .into_iter()
            .map(|d| WorkspaceDetail::new(d.label, d.value))
            .collect(),
    })
}

/// `StartOutcome` to a `StartWorkspaceResponse`.
pub fn start_outcome_to_proto(outcome: StartOutcome) -> Result<pb::StartWorkspaceResponse> {
    let (outcome, detail) = match outcome {
        StartOutcome::Running => (pb::StartOutcome::Running, String::new()),
        StartOutcome::Started { detail } => (pb::StartOutcome::Started, detail),
        StartOutcome::Recreated { detail } => (pb::StartOutcome::Recreated, detail),
        other => return Err(invalid(format!("start outcome {other:?} has no wire form"))),
    };
    Ok(pb::StartWorkspaceResponse {
        outcome: outcome.into(),
        detail,
    })
}

/// A `StartWorkspaceResponse` to `StartOutcome`.
pub fn start_outcome_from_proto(response: pb::StartWorkspaceResponse) -> Result<StartOutcome> {
    Ok(match response.outcome() {
        pb::StartOutcome::Running => StartOutcome::Running,
        pb::StartOutcome::Started => StartOutcome::Started {
            detail: response.detail,
        },
        pb::StartOutcome::Recreated => StartOutcome::Recreated {
            detail: response.detail,
        },
        pb::StartOutcome::Unspecified => return Err(invalid("start outcome: unspecified")),
    })
}

/// `WorkspaceList` to a `ListWorkspacesResponse`.
pub fn workspace_list_to_proto(list: WorkspaceList) -> Result<pb::ListWorkspacesResponse> {
    Ok(pb::ListWorkspacesResponse {
        workspaces: list
            .workspaces
            .into_iter()
            .map(workspace_to_proto)
            .collect::<Result<_>>()?,
        free_bytes: list.free_bytes,
    })
}

/// A `ListWorkspacesResponse` to `WorkspaceList`.
pub fn workspace_list_from_proto(response: pb::ListWorkspacesResponse) -> Result<WorkspaceList> {
    Ok(WorkspaceList {
        workspaces: response
            .workspaces
            .into_iter()
            .map(workspace_from_proto)
            .collect::<Result<_>>()?,
        free_bytes: response.free_bytes,
    })
}

/// A workspace id and an `ExecRequest` to the wire request.
pub fn exec_request_to_proto(id: &WorkspaceId, request: ExecRequest) -> pb::ExecRequest {
    pb::ExecRequest {
        workspace_id: id.to_string(),
        command: request.command,
        timeout: Some(duration_to_proto(request.timeout)),
        output_limit: request.output_limit,
    }
}

/// A wire `ExecRequest` to its workspace id and `ExecRequest`. The timeout is required.
pub fn exec_request_from_proto(request: pb::ExecRequest) -> Result<(WorkspaceId, ExecRequest)> {
    let timeout = request.timeout.ok_or_else(|| invalid("timeout: missing"))?;
    Ok((
        WorkspaceId::new(request.workspace_id)?,
        ExecRequest {
            command: request.command,
            timeout: duration_from_proto(timeout)?,
            output_limit: request.output_limit,
        },
    ))
}

/// `ExecEvent` to an `ExecResponse`.
pub fn exec_event_to_proto(event: ExecEvent) -> Result<pb::ExecResponse> {
    use pb::exec_response::Event;
    let event = match event {
        ExecEvent::Stdout(data) => Event::Stdout(data),
        ExecEvent::Stderr(data) => Event::Stderr(data),
        ExecEvent::Exit(exit) => Event::Exit(pb::ExecExit {
            code: exit.code,
            timed_out: exit.timed_out,
            dropped_bytes: exit.dropped_bytes,
        }),
        other => return Err(invalid(format!("exec event {other:?} has no wire form"))),
    };
    Ok(pb::ExecResponse { event: Some(event) })
}

/// An `ExecResponse` to `ExecEvent`. An unset or unknown event is invalid.
pub fn exec_event_from_proto(response: pb::ExecResponse) -> Result<ExecEvent> {
    use pb::exec_response::Event;
    Ok(match response.event {
        Some(Event::Stdout(data)) => ExecEvent::Stdout(data),
        Some(Event::Stderr(data)) => ExecEvent::Stderr(data),
        Some(Event::Exit(exit)) => ExecEvent::Exit(ExecExit {
            code: exit.code,
            timed_out: exit.timed_out,
            dropped_bytes: exit.dropped_bytes,
        }),
        None => return Err(invalid("exec event: missing or unknown")),
    })
}

/// A workspace id and a `FindRequest` to the wire request.
pub fn find_request_to_proto(id: &WorkspaceId, request: FindRequest) -> pb::FindRequest {
    pb::FindRequest {
        workspace_id: id.to_string(),
        pattern: request.pattern,
        path: request.path.unwrap_or_default(),
        limit: request.limit,
    }
}

/// A wire `FindRequest` to its workspace id and `FindRequest`.
pub fn find_request_from_proto(request: pb::FindRequest) -> Result<(WorkspaceId, FindRequest)> {
    Ok((
        WorkspaceId::new(request.workspace_id)?,
        FindRequest {
            pattern: request.pattern,
            path: non_empty(request.path),
            limit: request.limit,
        },
    ))
}

/// `FindResult` to a `FindResponse`.
pub fn find_result_to_proto(result: FindResult) -> pb::FindResponse {
    pb::FindResponse {
        paths: result.paths,
        truncated: result.truncated,
    }
}

/// A `FindResponse` to `FindResult`.
pub fn find_result_from_proto(response: pb::FindResponse) -> FindResult {
    FindResult {
        paths: response.paths,
        truncated: response.truncated,
    }
}

/// A workspace id and a `GrepRequest` to the wire request.
pub fn grep_request_to_proto(id: &WorkspaceId, request: GrepRequest) -> pb::GrepRequest {
    pb::GrepRequest {
        workspace_id: id.to_string(),
        pattern: request.pattern,
        path: request.path.unwrap_or_default(),
        glob: request.glob,
        case_insensitive: request.case_insensitive,
        limit: request.limit,
    }
}

/// A wire `GrepRequest` to its workspace id and `GrepRequest`.
pub fn grep_request_from_proto(request: pb::GrepRequest) -> Result<(WorkspaceId, GrepRequest)> {
    Ok((
        WorkspaceId::new(request.workspace_id)?,
        GrepRequest {
            pattern: request.pattern,
            path: non_empty(request.path),
            glob: request.glob,
            case_insensitive: request.case_insensitive,
            limit: request.limit,
        },
    ))
}

/// `GrepResult` to a `GrepResponse`.
pub fn grep_result_to_proto(result: GrepResult) -> pb::GrepResponse {
    pb::GrepResponse {
        matches: result
            .matches
            .into_iter()
            .map(|m| pb::GrepMatch {
                path: m.path,
                line: m.line,
                text: m.text,
            })
            .collect(),
        truncated: result.truncated,
    }
}

/// A `GrepResponse` to `GrepResult`.
pub fn grep_result_from_proto(response: pb::GrepResponse) -> GrepResult {
    GrepResult {
        matches: response
            .matches
            .into_iter()
            .map(|m| GrepMatch {
                path: m.path,
                line: m.line,
                text: m.text,
            })
            .collect(),
        truncated: response.truncated,
    }
}

/// `ReadFile` to a `ReadFileResponse`.
pub fn read_file_to_proto(file: ReadFile) -> pb::ReadFileResponse {
    pb::ReadFileResponse {
        content: file.content,
        size: file.size,
    }
}

/// A `ReadFileResponse` to `ReadFile`.
pub fn read_file_from_proto(response: pb::ReadFileResponse) -> ReadFile {
    ReadFile {
        content: response.content,
        size: response.size,
    }
}

/// The wire's empty string is a path that was left out.
fn non_empty(path: String) -> Option<String> {
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use giles_types::{Error, Timestamp};

    use super::*;

    fn id(s: &str) -> WorkspaceId {
        WorkspaceId::new(s).unwrap()
    }

    fn workspace() -> Workspace {
        Workspace {
            id: id("ws-1"),
            description: "debian@sha256:abc".into(),
            working_directory: "/workspace".into(),
            persistence: "/workspace survives; the rest is reset".into(),
            created_at: "2026-11-10T12:00:00.5Z".parse::<Timestamp>().unwrap(),
            state: WorkspaceState::Running,
            disk_used_bytes: Some(0),
            details: vec![
                WorkspaceDetail::new("Image", "debian:stable"),
                WorkspaceDetail::new("Container", "giles-main.ws-1"),
            ],
        }
    }

    #[test]
    fn durations_round_trip_and_refuse_negatives() {
        for d in [
            Duration::ZERO,
            Duration::new(90, 500),
            Duration::from_secs(u32::MAX.into()),
        ] {
            assert_eq!(duration_from_proto(duration_to_proto(d)).unwrap(), d);
        }
        assert_eq!(
            duration_to_proto(Duration::MAX),
            prost_types::Duration {
                seconds: i64::MAX,
                nanos: 999_999_999
            }
        );
        for (seconds, nanos) in [(-1, 0), (1, -1), (0, 1_000_000_000)] {
            let error = duration_from_proto(prost_types::Duration { seconds, nanos });
            assert!(
                matches!(error, Err(Error::InvalidRequest(_))),
                "{seconds} {nanos}"
            );
        }
    }

    #[test]
    fn workspaces_round_trip() {
        let wire = workspace_to_proto(workspace()).unwrap();
        assert_eq!(wire.workspace_id, "ws-1");
        assert_eq!(wire.state(), pb::WorkspaceState::Running);
        assert_eq!(
            wire.details,
            vec![
                pb::WorkspaceDetail {
                    label: "Image".into(),
                    value: "debian:stable".into()
                },
                pb::WorkspaceDetail {
                    label: "Container".into(),
                    value: "giles-main.ws-1".into()
                },
            ]
        );
        assert_eq!(workspace_from_proto(wire.clone()).unwrap(), workspace());

        let unknown_disk = Workspace {
            disk_used_bytes: None,
            details: Vec::new(),
            state: WorkspaceState::Stopped,
            ..workspace()
        };
        let wire = workspace_to_proto(unknown_disk.clone()).unwrap();
        assert_eq!(wire.disk_used_bytes, None);
        assert_eq!(workspace_from_proto(wire).unwrap(), unknown_disk);

        let list = WorkspaceList {
            workspaces: vec![workspace()],
            free_bytes: Some(10),
        };
        let wire = workspace_list_to_proto(list.clone()).unwrap();
        assert_eq!(workspace_list_from_proto(wire).unwrap(), list);
    }

    #[test]
    fn invalid_workspaces_are_refused() {
        let good = workspace_to_proto(workspace()).unwrap();
        let cases = [
            pb::Workspace {
                workspace_id: "Not Valid".into(),
                ..good.clone()
            },
            pb::Workspace {
                created_at: None,
                ..good.clone()
            },
            pb::Workspace {
                state: pb::WorkspaceState::Unspecified.into(),
                ..good.clone()
            },
            pb::Workspace { state: 99, ..good },
        ];
        for case in cases {
            assert!(workspace_from_proto(case.clone()).is_err(), "{case:?}");
        }
    }

    #[test]
    fn start_outcomes_round_trip() {
        for outcome in [
            StartOutcome::Running,
            StartOutcome::Started {
                detail: "was idle".into(),
            },
            StartOutcome::Recreated {
                detail: "packages are gone".into(),
            },
        ] {
            let wire = start_outcome_to_proto(outcome.clone()).unwrap();
            assert_eq!(start_outcome_from_proto(wire).unwrap(), outcome);
        }
        assert!(start_outcome_from_proto(pb::StartWorkspaceResponse::default()).is_err());
    }

    #[test]
    fn exec_requests_and_events_round_trip() {
        let request = ExecRequest {
            command: "make test".into(),
            timeout: Duration::from_secs(120),
            output_limit: 1 << 20,
        };
        let wire = exec_request_to_proto(&id("ws-1"), request.clone());
        assert_eq!(wire.workspace_id, "ws-1");
        assert_eq!(
            exec_request_from_proto(wire.clone()).unwrap(),
            (id("ws-1"), request)
        );
        let no_timeout = pb::ExecRequest {
            timeout: None,
            ..wire
        };
        assert!(exec_request_from_proto(no_timeout).is_err());

        for event in [
            ExecEvent::Stdout(b"out".to_vec()),
            ExecEvent::Stderr(b"err".to_vec()),
            ExecEvent::Exit(ExecExit {
                code: 137,
                timed_out: true,
                dropped_bytes: 5,
            }),
        ] {
            let wire = exec_event_to_proto(event.clone()).unwrap();
            assert_eq!(exec_event_from_proto(wire).unwrap(), event);
        }
        assert!(exec_event_from_proto(pb::ExecResponse::default()).is_err());
    }

    #[test]
    fn find_and_grep_map_an_empty_path_to_none() {
        let find = FindRequest {
            pattern: "**/*.rs".into(),
            path: None,
            limit: 100,
        };
        let wire = find_request_to_proto(&id("ws-1"), find.clone());
        assert_eq!(wire.path, "");
        assert_eq!(find_request_from_proto(wire).unwrap(), (id("ws-1"), find));

        let grep = GrepRequest {
            pattern: "todo".into(),
            path: Some("src".into()),
            glob: Some("*.rs".into()),
            case_insensitive: true,
            limit: 50,
        };
        let wire = grep_request_to_proto(&id("ws-1"), grep.clone());
        assert_eq!(wire.path, "src");
        assert_eq!(grep_request_from_proto(wire).unwrap(), (id("ws-1"), grep));

        let found = FindResult {
            paths: vec!["a.rs".into()],
            truncated: true,
        };
        assert_eq!(
            find_result_from_proto(find_result_to_proto(found.clone())),
            found
        );
        let matched = GrepResult {
            matches: vec![GrepMatch {
                path: "a.rs".into(),
                line: 3,
                text: "// TODO".into(),
            }],
            truncated: false,
        };
        assert_eq!(
            grep_result_from_proto(grep_result_to_proto(matched.clone())),
            matched
        );
        let file = ReadFile {
            content: b"hi".to_vec(),
            size: 10,
        };
        assert_eq!(read_file_from_proto(read_file_to_proto(file.clone())), file);
    }

    #[test]
    fn info_and_options_are_json() {
        let info = EnvironmentInfo {
            options_schema: serde_json::json!({"type": "object"}),
        };
        assert_eq!(info_from_proto(info_to_proto(&info)).unwrap(), info);
        assert!(info_from_proto(pb::InfoResponse::default()).is_err());

        assert_eq!(options_from_proto("").unwrap(), serde_json::json!({}));
        assert_eq!(
            options_from_proto(r#"{"image": "debian"}"#).unwrap(),
            serde_json::json!({"image": "debian"})
        );
        assert!(options_from_proto("[1]").is_err());
        assert!(options_from_proto("{").is_err());
    }
}
