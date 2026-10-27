//! The plugin binary's command line, as `ChannelPlugin::run` gives every shipped plugin: usage,
//! the mistakes it reports, and `serve` taking its transport settings from a `--config` file.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use giles_proto::handshake::{MAGIC_COOKIE, MAGIC_COOKIE_ENV};

const BINARY: &str = env!("CARGO_BIN_EXE_giles-channel-echo");

/// Runs the binary with `args` and exactly `env`, stdin closed.
fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(BINARY)
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn help_goes_to_stdout_and_succeeds() {
    for args in [&["--help"][..], &["-h"], &["serve", "--help"]] {
        let output = run(args, &[]);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        let stdout = text(&output.stdout);
        assert!(
            stdout.starts_with("Usage: giles-channel-echo <command>"),
            "{stdout}"
        );
        assert!(stdout.contains("GILES_CHANNEL_ECHO_TOKEN"), "{stdout}");
        assert_eq!(text(&output.stderr), "");
    }
}

#[test]
fn no_command_prints_the_usage_to_stderr() {
    let output = run(&[], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    assert!(text(&output.stderr).starts_with("Usage: giles-channel-echo <command>"));
}

#[test]
fn a_core_that_launches_it_without_socket_gets_a_clear_error() {
    let output = run(&[], &[(MAGIC_COOKIE_ENV, MAGIC_COOKIE)]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("without the `socket` argument") && stderr.contains("upgrade Giles"),
        "{stderr}"
    );
}

#[test]
fn mistakes_name_themselves_and_exit_2() {
    let cases: &[(&[&str], &str)] = &[
        (&["launch"], "unknown command \"launch\""),
        (&["socket", "x"], "`socket` takes no argument \"x\""),
        (&["serve", "--port", "1"], "unknown option \"--port\""),
        (
            &["serve", "--data", "d", "--no-auth"],
            "no listen: set `listen` in the --config file or pass --listen",
        ),
        (
            &["serve", "--listen", "127.0.0.1:0", "--no-auth"],
            "no data: set `data` in the --config file or pass --data",
        ),
        (
            &[
                "serve",
                "--listen=127.0.0.1:0",
                "--data=d",
                "--tls-cert=c.pem",
                "--no-auth",
            ],
            "`tls_cert` and `tls_key` go together",
        ),
        (
            &["serve", "--listen=127.0.0.1:0", "--data=d"],
            "no bearer token: set GILES_CHANNEL_ECHO_TOKEN",
        ),
    ];
    for (args, message) in cases {
        let output = run(args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with("giles-channel-echo: ") && stderr.contains(message),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn serve_reads_its_transport_settings_from_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, contents: &str, mode: u32| {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    let token = write("token", "s3cret\n", 0o600);
    let config = write(
        "serve.toml",
        &format!(
            "listen = \"127.0.0.1:0\"\ndata = {:?}\ntoken_file = {:?}\n",
            dir.path().join("data"),
            token
        ),
        0o644,
    );
    let mut child = Command::new(BINARY)
        .args(["serve", "--config"])
        .arg(&config)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(line.starts_with("listening on 127.0.0.1:"), "{line:?}");
}

#[test]
fn a_token_file_others_can_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = run(
        &[
            "serve",
            "--listen=127.0.0.1:0",
            "--data",
            dir.path().join("data").to_str().unwrap(),
            "--token-file",
            token.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("mode 0644"), "{stderr}");
    assert!(!Path::new(&dir.path().join("data")).exists());
}
