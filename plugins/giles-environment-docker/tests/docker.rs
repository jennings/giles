//! The Docker backend against a real Docker API socket (Docker, or Podman's compatible API). Every
//! test skips, saying why, when no socket answers; set `GILES_REQUIRE_DOCKER=1` to fail instead.
//! The Linux helper is cross-built for the engine's architecture (`GILES_HELPER_DIR` names a
//! directory that already has it).

mod support;

use std::time::{Duration, Instant};

use bollard::models::ContainerSummaryStateEnum;
use bollard::query_parameters::{ListContainersOptionsBuilder, ListVolumesOptionsBuilder};
use futures::StreamExt;
use giles_environment_docker::{Config, DockerEnvironment};
use giles_types::{
    Environment, Error, ExecEvent, ExecRequest, FindRequest, GrepRequest, StartOutcome,
    WorkspaceState,
};
use serde_json::json;
use support::{IMAGE, harness, wid};

macro_rules! scope {
    ($name:expr) => {{
        let Some(harness) = harness() else { return };
        let scope = harness.scope($name, harness.config()).await;
        (harness, scope)
    }};
    ($name:expr, $config:expr) => {{
        let Some(harness) = harness() else { return };
        let scope = harness.scope($name, $config(harness.config())).await;
        (harness, scope)
    }};
}

/// Polls `check` until it returns true or ten seconds pass.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn labeled(docker: &bollard::Docker, instance_label: &str) -> (usize, usize) {
    let filters =
        std::collections::HashMap::from([("label".to_owned(), vec![instance_label.to_owned()])]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .unwrap()
        .len();
    let volumes = docker
        .list_volumes(Some(
            ListVolumesOptionsBuilder::default()
                .filters(&filters)
                .build(),
        ))
        .await
        .unwrap()
        .volumes
        .unwrap_or_default()
        .len();
    (containers, volumes)
}

#[tokio::test]
async fn info_lists_the_allowed_images_and_takes_only_an_image() {
    let (_, scope) = scope!("info", |mut c: Config| {
        c.images = vec!["busybox:1.37".into()];
        c
    });
    let info = scope.env.info().await.unwrap();
    let schema = info.options_schema;
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
    let description = schema["properties"]["image"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("alpine:*"), "{description}");
    assert!(description.contains("busybox:1.37"), "{description}");
}

#[tokio::test]
async fn create_refuses_images_outside_the_allowlist_and_bad_options() {
    let (_, scope) = scope!("refuse");
    let error = scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": "busybox" }))
        .await
        .unwrap_err();
    let Error::InvalidRequest(message) = error else {
        panic!("expected InvalidRequest, got {error:?}");
    };
    assert!(message.contains("busybox"), "{message}");
    assert!(
        message.contains("ubuntu:*") && message.contains("alpine:*"),
        "{message}"
    );

    for options in [
        json!({}),
        json!({ "image": IMAGE, "privileged": true }),
        json!({ "image": 7 }),
        json!({ "image": "not a reference" }),
        json!({ "image": "alpine:*" }),
    ] {
        let error = scope
            .env
            .create_workspace(&wid("w1"), options.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(_)),
            "{options}: {error:?}"
        );
    }
    // The id type refuses a bad id before the plugin sees it.
    assert!(giles_types::WorkspaceId::new("Bad_Id").is_err());
    assert!(
        scope
            .env
            .list_workspaces()
            .await
            .unwrap()
            .workspaces
            .is_empty()
    );
}

#[tokio::test]
async fn create_then_exec_streams_output_and_reports_the_exit() {
    let (_, scope) = scope!("exec", |mut c: Config| {
        c.env.insert("GILES_TEST_VAR".into(), "hello".into());
        c
    });
    let workspace = scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": IMAGE }))
        .await
        .unwrap();
    assert_eq!(workspace.id.as_str(), "w1");
    assert_eq!(workspace.working_directory, "/workspace");
    assert_eq!(workspace.state, WorkspaceState::Running);
    assert!(
        workspace.description.contains("alpine:3.20"),
        "{}",
        workspace.description
    );
    assert!(workspace.persistence.contains("/workspace"));

    let ran = scope.run("w1", "echo out; echo err >&2; exit 3").await;
    assert_eq!(
        (ran.stdout.as_str(), ran.stderr.as_str(), ran.code),
        ("out\n", "err\n", 3)
    );
    assert!(!ran.timed_out);
    assert_eq!(ran.dropped, 0);

    let ran = scope
        .run(
            "w1",
            "pwd; echo $HOME; echo $GILES_TEST_VAR; id -u; ls -d /workspace/.home",
        )
        .await;
    assert_eq!(
        ran.stdout, "/workspace\n/workspace/.home\nhello\n0\n/workspace/.home\n",
        "{ran:?}"
    );
    assert_eq!(ran.code, 0);

    // A command killed by a signal exits with 128 + N.
    assert_eq!(scope.run("w1", "kill -9 $$").await.code, 137);
    // Files persist between commands.
    scope.run("w1", "echo kept > f.txt").await;
    assert_eq!(scope.run("w1", "cat f.txt").await.stdout, "kept\n");
    // A command that ends doesn't wait for the background processes it started.
    let started = Instant::now();
    let ran = scope.run("w1", "sleep 30 & echo launched").await;
    assert_eq!(ran.stdout, "launched\n");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[tokio::test]
async fn create_rejects_an_id_in_use() {
    let (_, scope) = scope!("dup");
    scope.workspace("w1").await;
    let error = scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": IMAGE }))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(m) if m.contains("already exists")));
}

#[tokio::test]
async fn output_past_the_limit_is_counted_not_sent() {
    let (_, scope) = scope!("limit");
    scope.workspace("w1").await;
    let ran = scope
        .run_with(
            "w1",
            "head -c 100000 /dev/zero | tr '\\0' x; echo done >&2",
            Duration::from_secs(30),
            1000,
        )
        .await;
    assert_eq!(ran.stdout.len() + ran.stderr.len(), 1000, "{ran:?}");
    assert_eq!(ran.dropped, 100_000 + 5 - 1000);
    assert_eq!(ran.code, 0);
}

#[tokio::test]
async fn a_command_past_its_timeout_is_killed() {
    let (_, scope) = scope!("timeout");
    scope.workspace("w1").await;
    let started = Instant::now();
    let ran = scope
        .run_with(
            "w1",
            "echo begin; sleep 300; echo never",
            Duration::from_secs(2),
            1 << 20,
        )
        .await;
    assert!(ran.timed_out, "{ran:?}");
    assert_eq!(ran.code, 137);
    assert_eq!(ran.stdout, "begin\n");
    assert!(started.elapsed() < Duration::from_secs(30));
    // The process is really gone, not just abandoned.
    let alive = scope
        .run("w1", "ps -o args | grep -c '^sleep 300'; true")
        .await;
    assert_eq!(alive.stdout.trim(), "0", "{alive:?}");
}

#[tokio::test]
async fn dropping_the_exec_stream_kills_the_command_and_its_children() {
    let (_, scope) = scope!("drop");
    scope.workspace("w1").await;
    let mut events = scope
        .env
        .exec(
            &wid("w1"),
            ExecRequest {
                command: "touch /workspace/started; (sleep 301 &) ; sleep 302".to_owned(),
                timeout: Duration::from_secs(120),
                output_limit: 1 << 20,
            },
        )
        .await
        .unwrap();
    // Wait until the command is really running, then walk away from it.
    eventually("the command to start", || async {
        scope.run("w1", "test -e /workspace/started").await.code == 0
    })
    .await;
    assert_eq!(
        scope
            .run("w1", "ps -o args | grep -c '^sleep 30[12]'")
            .await
            .stdout
            .trim(),
        "2"
    );
    drop(events.next());
    drop(events);
    eventually("the command to be killed", || async {
        scope
            .run("w1", "ps -o args | grep -c '^sleep 30[12]'; true")
            .await
            .stdout
            .trim()
            == "0"
    })
    .await;
}

#[tokio::test]
async fn cancelling_before_the_first_event_still_kills_the_command() {
    let (_, scope) = scope!("cancel");
    scope.workspace("w1").await;
    // Drop the whole exec future almost immediately, at a random point in its setup.
    let w1 = wid("w1");
    for delay_ms in [0u64, 5, 20] {
        let exec = scope.env.exec(
            &w1,
            ExecRequest {
                command: "sleep 303".to_owned(),
                timeout: Duration::from_secs(120),
                output_limit: 1024,
            },
        );
        let _ = tokio::time::timeout(Duration::from_millis(delay_ms), exec).await;
    }
    eventually("no sleep to remain", || async {
        scope
            .run("w1", "ps -o args | grep -c '^sleep 303'; true")
            .await
            .stdout
            .trim()
            == "0"
    })
    .await;
}

#[tokio::test]
async fn files_can_be_written_read_found_and_searched() {
    let (_, scope) = scope!("files");
    scope.workspace("w1").await;
    let env = &scope.env;

    env.write_file(&wid("w1"), "notes/a.txt", b"hello\nneedle one\n".to_vec())
        .await
        .unwrap();
    env.write_file(
        &wid("w1"),
        "/workspace/b.rs",
        b"fn needle() {}\r\nfn other() {}\n".to_vec(),
    )
    .await
    .unwrap();
    env.write_file(&wid("w1"), "-dash.txt", b"-".to_vec())
        .await
        .unwrap();
    assert_eq!(
        scope.run("w1", "cat notes/a.txt").await.stdout,
        "hello\nneedle one\n"
    );
    assert_eq!(scope.run("w1", "cat ./-dash.txt").await.stdout, "-");

    // Relative paths resolve against /workspace; absolute ones work too.
    let read = env
        .read_file(&wid("w1"), "notes/a.txt", 1000)
        .await
        .unwrap();
    assert_eq!(
        (read.content.as_slice(), read.size),
        (&b"hello\nneedle one\n"[..], 17)
    );
    let read = env
        .read_file(&wid("w1"), "/workspace/notes/a.txt", 5)
        .await
        .unwrap();
    assert_eq!((read.content.as_slice(), read.size), (&b"hello"[..], 17));
    let read = env.read_file(&wid("w1"), "notes/a.txt", 0).await.unwrap();
    assert_eq!((read.content.len(), read.size), (0, 17));
    assert!(matches!(
        env.read_file(&wid("w1"), "missing", 10).await,
        Err(Error::NotFound(_))
    ));
    assert!(
        matches!(env.read_file(&wid("w1"), "notes", 10).await, Err(Error::InvalidRequest(m)) if m.contains("directory"))
    );

    // An existing file keeps its permissions; a new one doesn't need to be executable.
    scope
        .run("w1", "printf '#!/bin/sh\\n' > run.sh; chmod 750 run.sh")
        .await;
    env.write_file(&wid("w1"), "run.sh", b"#!/bin/sh\necho hi\n".to_vec())
        .await
        .unwrap();
    assert_eq!(
        scope.run("w1", "stat -c %a run.sh; ./run.sh").await.stdout,
        "750\nhi\n"
    );

    // Binary content, empty content, and content bigger than a pipe buffer survive intact.
    let binary: Vec<u8> = (0..=255u8).cycle().take(3 * 1024 * 1024 + 7).collect();
    env.write_file(&wid("w1"), "big.bin", binary.clone())
        .await
        .unwrap();
    let read = env
        .read_file(&wid("w1"), "big.bin", u64::MAX)
        .await
        .unwrap();
    assert_eq!(read.size, binary.len() as u64);
    assert!(read.content == binary, "binary content changed");
    env.write_file(&wid("w1"), "empty", Vec::new())
        .await
        .unwrap();
    assert_eq!(
        env.read_file(&wid("w1"), "empty", 10).await.unwrap().size,
        0
    );

    // find: sorted, relative to the searched directory, .git and .gitignore skipped.
    env.write_file(&wid("w1"), ".gitignore", b"ignored.txt\n".to_vec())
        .await
        .unwrap();
    env.write_file(&wid("w1"), "ignored.txt", b"needle\n".to_vec())
        .await
        .unwrap();
    env.write_file(&wid("w1"), ".git/config", b"needle\n".to_vec())
        .await
        .unwrap();
    let found = env
        .find(
            &wid("w1"),
            FindRequest {
                pattern: "**/*.txt".into(),
                path: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(found.paths, ["-dash.txt", "notes/a.txt"]);
    assert!(!found.truncated);
    let found = env
        .find(
            &wid("w1"),
            FindRequest {
                pattern: "*".into(),
                path: Some("notes".into()),
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(found.paths, ["a.txt"]);
    let found = env
        .find(
            &wid("w1"),
            FindRequest {
                pattern: "**".into(),
                path: None,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(found.paths.len(), 2);
    assert!(found.truncated);
    assert!(matches!(
        env.find(
            &wid("w1"),
            FindRequest {
                pattern: "*".into(),
                path: Some("nope".into()),
                limit: 1
            }
        )
        .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        env.find(
            &wid("w1"),
            FindRequest {
                pattern: "[".into(),
                path: None,
                limit: 1
            }
        )
        .await,
        Err(Error::InvalidRequest(_))
    ));

    // grep: path order then line order, binary files, .git, and ignored files skipped.
    let w1 = wid("w1");
    let grep = |pattern: &str, path: Option<&str>, glob: Option<&str>, ci: bool, limit: u32| {
        env.grep(
            &w1,
            GrepRequest {
                pattern: pattern.into(),
                path: path.map(Into::into),
                glob: glob.map(Into::into),
                case_insensitive: ci,
                limit,
            },
        )
    };
    let result = grep("needle", None, None, false, 100).await.unwrap();
    let hits: Vec<_> = result
        .matches
        .iter()
        .map(|m| (m.path.as_str(), m.line, m.text.as_str()))
        .collect();
    assert_eq!(
        hits,
        [
            ("b.rs", 1, "fn needle() {}"),
            ("notes/a.txt", 2, "needle one")
        ]
    );
    assert!(!result.truncated);
    let result = grep("NEEDLE", None, Some("**/*.txt"), true, 100)
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    let result = grep("needle", Some("notes/a.txt"), None, false, 100)
        .await
        .unwrap();
    assert_eq!(result.matches[0].path, "a.txt");
    let result = grep("needle", None, None, false, 1).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.truncated);
    assert!(matches!(
        grep("(", None, None, false, 1).await,
        Err(Error::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn stopping_keeps_files_and_starting_says_processes_ended() {
    let (_, scope) = scope!("stop");
    scope.workspace("w1").await;
    scope
        .run(
            "w1",
            "echo persisted > /workspace/keep.txt; echo in-container > /var/tmp/layer.txt",
        )
        .await;
    assert!(matches!(
        scope.env.start_workspace(&wid("w1")).await.unwrap(),
        StartOutcome::Running
    ));

    scope.env.stop_workspace(&wid("w1")).await.unwrap();
    // Stopping twice is fine.
    scope.env.stop_workspace(&wid("w1")).await.unwrap();
    let list = scope.env.list_workspaces().await.unwrap();
    assert_eq!(list.workspaces.len(), 1);
    assert_eq!(list.workspaces[0].state, WorkspaceState::Stopped);
    // Running commands in a stopped workspace is an error, not a silent start.
    assert!(matches!(
        scope
            .env
            .exec(
                &wid("w1"),
                ExecRequest {
                    command: "true".into(),
                    timeout: Duration::from_secs(5),
                    output_limit: 10
                }
            )
            .await,
        Err(Error::InvalidRequest(_))
    ));

    let outcome = scope.env.start_workspace(&wid("w1")).await.unwrap();
    let StartOutcome::Started { detail } = outcome else {
        panic!("expected Started, got {outcome:?}");
    };
    assert!(detail.contains("ended"), "{detail}");
    assert_eq!(
        scope
            .run("w1", "cat /workspace/keep.txt /var/tmp/layer.txt")
            .await
            .stdout,
        "persisted\nin-container\n"
    );
    let list = scope.env.list_workspaces().await.unwrap();
    assert_eq!(list.workspaces[0].state, WorkspaceState::Running);
    assert!(matches!(
        scope.env.start_workspace(&wid("w1")).await.unwrap(),
        StartOutcome::Running
    ));
}

#[tokio::test]
async fn a_container_removed_outside_is_recreated_with_its_files() {
    let (_, scope) = scope!("recreate");
    scope.workspace("w1").await;
    scope.run("w1", "echo precious > /workspace/data.txt; mkdir -p /workspace/.home; echo cfg > $HOME/.rc; echo gone > /var/tmp/installed.txt").await;
    let container = scope
        .docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default().all(true).build(),
        ))
        .await
        .unwrap()
        .into_iter()
        .find(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get("giles.instance"))
                .map(String::as_str)
                == Some(scope_instance(&scope).as_str())
        })
        .expect("the workspace's container");
    scope
        .docker
        .remove_container(
            container.id.as_deref().unwrap(),
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .build(),
            ),
        )
        .await
        .unwrap();

    // The volume is still there, so the workspace is listed (stopped), and not lost.
    let list = scope.env.list_workspaces().await.unwrap();
    assert_eq!(list.workspaces.len(), 1);
    assert_eq!(list.workspaces[0].state, WorkspaceState::Stopped);

    let outcome = scope.env.start_workspace(&wid("w1")).await.unwrap();
    let StartOutcome::Recreated { detail } = outcome else {
        panic!("expected Recreated, got {outcome:?}");
    };
    assert!(
        detail.contains("/workspace") && detail.contains("apt"),
        "{detail}"
    );
    let ran = scope
        .run(
            "w1",
            "cat /workspace/data.txt; cat $HOME/.rc; test -e /var/tmp/installed.txt; echo $?",
        )
        .await;
    assert_eq!(ran.stdout, "precious\ncfg\n1\n", "{ran:?}");
    assert!(matches!(
        scope.env.start_workspace(&wid("w1")).await.unwrap(),
        StartOutcome::Running
    ));
}

/// The instance name behind a scope: its workspaces' `giles.instance` label.
fn scope_instance(scope: &support::Scope) -> String {
    scope.instance.clone()
}

#[tokio::test]
async fn concurrent_starts_recreate_the_container_once() {
    let (_, scope) = scope!("race");
    scope.workspace("w1").await;
    let container = scope
        .docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default().all(true).build(),
        ))
        .await
        .unwrap()
        .into_iter()
        .find(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get("giles.instance"))
                .map(String::as_str)
                == Some(scope_instance(&scope).as_str())
        })
        .unwrap();
    scope
        .docker
        .remove_container(
            container.id.as_deref().unwrap(),
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .build(),
            ),
        )
        .await
        .unwrap();
    let w1 = wid("w1");
    let starts = futures::future::join_all((0..5).map(|_| scope.env.start_workspace(&w1))).await;
    let recreated = starts
        .iter()
        .filter(|s| matches!(s.as_ref().unwrap(), StartOutcome::Recreated { .. }))
        .count();
    assert_eq!(recreated, 1, "{starts:?}");
}

#[tokio::test]
async fn delete_removes_the_container_and_volume_and_is_repeatable() {
    let (_, scope) = scope!("delete");
    scope.workspace("w1").await;
    scope.workspace("w2").await;
    let label = format!("giles.instance={}", scope_instance(&scope));
    assert_eq!(labeled(&scope.docker, &label).await, (2, 2));

    scope.env.delete_workspace(&wid("w1")).await.unwrap();
    assert_eq!(labeled(&scope.docker, &label).await, (1, 1));
    let list = scope.env.list_workspaces().await.unwrap();
    assert_eq!(
        list.workspaces
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>(),
        ["w2"]
    );

    // A missing workspace deletes fine; using it is NotFound.
    scope.env.delete_workspace(&wid("w1")).await.unwrap();
    assert!(matches!(
        scope.env.start_workspace(&wid("w1")).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        scope.env.stop_workspace(&wid("w1")).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        scope.env.read_file(&wid("w1"), "x", 1).await,
        Err(Error::NotFound(_))
    ));

    // A stopped workspace, and one whose container is already gone, delete too.
    scope.env.stop_workspace(&wid("w2")).await.unwrap();
    scope.env.delete_workspace(&wid("w2")).await.unwrap();
    assert_eq!(labeled(&scope.docker, &label).await, (0, 0));
}

#[tokio::test]
async fn an_image_the_plugin_pulled_is_removed_with_its_last_workspace() {
    const BUSYBOX: &str = "busybox:1.37";
    let (_, scope) = scope!("pulled", |mut c: Config| {
        c.images = vec![BUSYBOX.into()];
        c
    });
    // Start without the image, so the plugin pulls it.
    let _ = scope
        .docker
        .remove_image(
            BUSYBOX,
            None::<bollard::query_parameters::RemoveImageOptions>,
            None,
        )
        .await;
    scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": BUSYBOX }))
        .await
        .unwrap();
    scope
        .env
        .create_workspace(&wid("w2"), json!({ "image": BUSYBOX }))
        .await
        .unwrap();

    scope.env.delete_workspace(&wid("w1")).await.unwrap();
    assert!(
        scope.docker.inspect_image(BUSYBOX).await.is_ok(),
        "w2 still uses it"
    );
    scope.env.delete_workspace(&wid("w2")).await.unwrap();
    assert!(
        scope.docker.inspect_image(BUSYBOX).await.is_err(),
        "the last workspace is gone"
    );

    // An image the engine already had stays.
    scope
        .env
        .create_workspace(&wid("w3"), json!({ "image": BUSYBOX }))
        .await
        .unwrap();
    // (It was pulled by w3, so make it "preexisting" for the next instance.)
    let other = support::harness()
        .unwrap()
        .scope("pulled-other", {
            let mut c = support::harness().unwrap().config();
            c.images = vec![BUSYBOX.into()];
            c
        })
        .await;
    other
        .env
        .create_workspace(&wid("w4"), json!({ "image": BUSYBOX }))
        .await
        .unwrap();
    other.env.delete_workspace(&wid("w4")).await.unwrap();
    assert!(
        scope.docker.inspect_image(BUSYBOX).await.is_ok(),
        "another instance's workspace may not remove an image it didn't pull"
    );
    scope.env.delete_workspace(&wid("w3")).await.unwrap();
    assert!(scope.docker.inspect_image(BUSYBOX).await.is_err());
}

#[tokio::test]
async fn instances_do_not_see_each_others_workspaces() {
    let Some(harness) = harness() else { return };
    let a = harness.scope("iso-a", harness.config()).await;
    let b = harness.scope("iso-b", harness.config()).await;
    a.workspace("w1").await;
    assert!(b.env.list_workspaces().await.unwrap().workspaces.is_empty());
    assert!(matches!(
        b.env.start_workspace(&wid("w1")).await,
        Err(Error::NotFound(_))
    ));
    b.env.delete_workspace(&wid("w1")).await.unwrap();
    assert_eq!(a.env.list_workspaces().await.unwrap().workspaces.len(), 1);
    // The same id in both is fine.
    b.workspace("w1").await;
    assert_eq!(scope_ids(&a).await, ["w1"]);
}

async fn scope_ids(scope: &support::Scope) -> Vec<String> {
    scope
        .env
        .list_workspaces()
        .await
        .unwrap()
        .workspaces
        .into_iter()
        .map(|w| w.id.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn list_reports_workspaces_with_disk_and_free_space() {
    let (_, scope) = scope!("list");
    scope.workspace("w2").await;
    scope.workspace("w1").await;
    scope
        .run("w1", "head -c 2000000 /dev/urandom > /workspace/blob")
        .await;
    let list = scope.env.list_workspaces().await.unwrap();
    assert_eq!(
        list.workspaces
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>(),
        ["w1", "w2"]
    );
    assert!(
        list.free_bytes.is_some_and(|b| b > 0),
        "{:?}",
        list.free_bytes
    );
    let w1 = &list.workspaces[0];
    assert_eq!(w1.state, WorkspaceState::Running);
    // Some engines can't measure a volume; those that can, count what is in it.
    if let Some(used) = w1.disk_used_bytes {
        assert!(used >= 2_000_000, "{used}");
    }
    assert!(w1.created_at.as_second() > 1_700_000_000);
}

#[tokio::test]
async fn containers_get_the_configured_limits_and_nothing_extra() {
    let (_, scope) = scope!("limits", |mut c: Config| {
        c.memory = Some("256m".into());
        c.cpus = Some(0.5);
        c.pids = Some(200);
        c.network = "none".into();
        c
    });
    scope.workspace("w1").await;
    let instance = scope_instance(&scope);
    let container = scope
        .docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default().all(true).build(),
        ))
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.labels.as_ref().and_then(|l| l.get("giles.instance")) == Some(&instance))
        .unwrap();
    assert_eq!(container.state, Some(ContainerSummaryStateEnum::RUNNING));
    let inspect = scope
        .docker
        .inspect_container(
            container.id.as_deref().unwrap(),
            None::<bollard::query_parameters::InspectContainerOptions>,
        )
        .await
        .unwrap();
    let host = inspect.host_config.unwrap();
    assert_eq!(host.memory, Some(256 << 20));
    assert_eq!(host.nano_cpus, Some(500_000_000));
    assert_eq!(host.pids_limit, Some(200));
    assert_eq!(host.network_mode.as_deref(), Some("none"));
    assert_eq!(host.init, Some(true));
    assert_ne!(host.privileged, Some(true));
    assert!(host.cap_add.unwrap_or_default().is_empty());
    assert!(host.devices.unwrap_or_default().is_empty());
    let labels = inspect.config.unwrap().labels.unwrap();
    assert_eq!(labels["giles.workspace"], "w1");
    assert!(
        labels["giles.image-digest"].starts_with("docker.io/library/alpine@sha256:")
            || labels["giles.image-digest"].starts_with("sha256:"),
        "{labels:?}"
    );
    // No network at all.
    let ran = scope
        .run("w1", "cat /sys/class/net/*/operstate | grep -c up; true")
        .await;
    assert_eq!(ran.stdout.trim(), "0", "{ran:?}");
}

#[tokio::test]
async fn connecting_without_a_helper_for_the_engine_fails_clearly() {
    let Some(harness) = harness() else { return };
    let empty = std::env::temp_dir().join(format!("giles-empty-helper-dir-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let config = Config {
        helper_dir: Some(empty),
        ..harness.config()
    };
    let error = DockerEnvironment::connect(config, "x").await.unwrap_err();
    let Error::InvalidRequest(message) = error else {
        panic!("expected InvalidRequest, got {error:?}");
    };
    assert!(message.contains("giles-workspace-helper-"), "{message}");
    assert!(message.contains("musl"), "{message}");
}

#[tokio::test]
async fn connecting_to_a_missing_socket_is_unavailable() {
    let config = Config {
        socket: Some("/nonexistent/docker.sock".into()),
        helper_dir: Some(std::env::temp_dir()),
        ..Config::default()
    };
    let error = DockerEnvironment::connect(config, "x").await.unwrap_err();
    assert!(
        matches!(error, Error::Unavailable { message: m, .. } if m.contains("/nonexistent/docker.sock"))
    );
}

#[tokio::test]
async fn a_digest_pinned_image_is_allowed_by_a_star_pattern() {
    let (_, scope) = scope!("digest");
    let inspect = scope.docker.inspect_image(IMAGE).await.unwrap();
    let Some(digest) = inspect.repo_digests.unwrap_or_default().into_iter().next() else {
        eprintln!("skipping: the engine lists no repository digest for {IMAGE}");
        return;
    };
    let (_, hex) = digest.split_once('@').unwrap();
    let workspace = scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": format!("alpine@{hex}") }))
        .await
        .unwrap();
    assert!(
        workspace.description.contains(hex),
        "{}",
        workspace.description
    );
    // A literal-tag pattern can't vouch for a digest.
    let (_, strict) = scope!("digest-strict", |mut c: Config| {
        c.default_images = false;
        c.images = vec!["alpine:3.20".into()];
        c
    });
    let error = strict
        .env
        .create_workspace(&wid("w1"), json!({ "image": format!("alpine@{hex}") }))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)));
}

#[tokio::test]
async fn exec_events_end_with_exactly_one_exit() {
    let (_, scope) = scope!("events");
    scope.workspace("w1").await;
    let mut events = scope
        .env
        .exec(
            &wid("w1"),
            ExecRequest {
                command: "for i in 1 2 3; do echo $i; sleep 0.2; done".into(),
                timeout: Duration::from_secs(30),
                output_limit: 1 << 20,
            },
        )
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        seen.push(event.unwrap());
    }
    // Output arrives as the command writes it, not all at the end.
    let outputs = seen
        .iter()
        .filter(|e| matches!(e, ExecEvent::Stdout(_)))
        .count();
    assert!(outputs >= 2, "{seen:?}");
    assert!(matches!(seen.last(), Some(ExecEvent::Exit(e)) if e.code == 0));
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, ExecEvent::Exit(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_image_without_a_shell_is_refused_and_leaves_nothing_behind() {
    const NO_SHELL: &str = "hello-world:linux";
    let (_, scope) = scope!("noshell", |mut c: Config| {
        c.images = vec![NO_SHELL.into()];
        c
    });
    let _ = scope
        .docker
        .remove_image(
            NO_SHELL,
            None::<bollard::query_parameters::RemoveImageOptions>,
            None,
        )
        .await;
    let error = scope
        .env
        .create_workspace(&wid("w1"), json!({ "image": NO_SHELL }))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::InvalidRequest(m) if m.contains("/bin/sh")),
        "{error:?}"
    );
    let label = format!("giles.instance={}", scope_instance(&scope));
    assert_eq!(labeled(&scope.docker, &label).await, (0, 0));
    assert!(
        scope
            .env
            .list_workspaces()
            .await
            .unwrap()
            .workspaces
            .is_empty()
    );
    // The image this attempt pulled didn't stay behind.
    assert!(scope.docker.inspect_image(NO_SHELL).await.is_err());
}

#[tokio::test]
async fn a_zero_timeout_is_an_invalid_request() {
    let (_, scope) = scope!("zero-timeout");
    scope.workspace("w1").await;
    let result = scope
        .env
        .exec(
            &wid("w1"),
            ExecRequest {
                command: "true".into(),
                timeout: Duration::ZERO,
                output_limit: 10,
            },
        )
        .await;
    assert!(matches!(result, Err(Error::InvalidRequest(m)) if m.contains("timeout")));
}
