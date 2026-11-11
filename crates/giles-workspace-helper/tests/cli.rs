//! Runs the built helper binary the way the Docker environment plugin does.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

fn helper(run_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_giles-workspace-helper"));
    command.env("GILES_RUN_DIR", run_dir);
    command
}

fn lines(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn tree(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    dir
}

fn find(dir: &Path, args: &[&str]) -> (Vec<String>, bool) {
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .arg("find")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    let mut values = lines(&out);
    let truncated = values.pop().unwrap()["truncated"].as_bool().unwrap();
    let paths = values
        .iter()
        .map(|v| v["path"].as_str().unwrap().to_owned())
        .collect();
    (paths, truncated)
}

#[test]
fn find_matches_a_glob_in_sorted_order() {
    let dir = tree(&[
        ("src/b.rs", ""),
        ("src/a.rs", ""),
        ("src/sub/c.rs", ""),
        ("src-extra/d.rs", ""),
        ("top.rs", ""),
        ("notes.txt", ""),
    ]);
    let (paths, truncated) = find(dir.path(), &["--pattern", "**/*.rs", "--limit", "100"]);
    assert_eq!(
        paths,
        [
            "src-extra/d.rs",
            "src/a.rs",
            "src/b.rs",
            "src/sub/c.rs",
            "top.rs"
        ]
    );
    assert!(!truncated);
}

#[test]
fn find_star_stays_within_a_directory() {
    let dir = tree(&[("a.rs", ""), ("src/b.rs", "")]);
    let (paths, _) = find(dir.path(), &["--pattern", "*.rs", "--limit", "10"]);
    assert_eq!(paths, ["a.rs"]);
}

#[test]
fn find_reports_paths_relative_to_the_searched_directory() {
    let dir = tree(&[("src/a.rs", ""), ("other/b.rs", "")]);
    let (paths, _) = find(
        dir.path(),
        &["--pattern", "**/*.rs", "--path", "src", "--limit", "10"],
    );
    assert_eq!(paths, ["a.rs"]);
}

#[test]
fn find_truncates_at_the_limit() {
    let dir = tree(&[("a", ""), ("b", ""), ("c", "")]);
    let (paths, truncated) = find(dir.path(), &["--pattern", "*", "--limit", "2"]);
    assert_eq!(paths, ["a", "b"]);
    assert!(truncated);
    let (paths, truncated) = find(dir.path(), &["--pattern", "*", "--limit", "3"]);
    assert_eq!(paths.len(), 3);
    assert!(!truncated);
}

#[test]
fn find_skips_git_and_gitignored_files_but_not_dotfiles() {
    // No `.git` directory: .gitignore applies anyway.
    let dir = tree(&[
        (".gitignore", "target/\n*.log\n"),
        (".git/config", ""),
        (".env", ""),
        ("target/out.bin", ""),
        ("run.log", ""),
        ("src/keep.rs", ""),
        ("src/.gitignore", "secret.rs\n"),
        ("src/secret.rs", ""),
    ]);
    let (paths, _) = find(dir.path(), &["--pattern", "**", "--limit", "100"]);
    assert_eq!(
        paths,
        [".env", ".gitignore", "src/.gitignore", "src/keep.rs"]
    );
}

#[test]
fn find_fails_on_a_missing_directory_and_a_bad_glob() {
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args([
            "find",
            "--pattern",
            "*",
            "--path",
            "/nonexistent-dir",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(66));
    let out = helper(run_dir.path())
        .args(["find", "--pattern", "[", "--limit", "1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid glob"));
}

fn grep(dir: &Path, args: &[&str]) -> (Vec<(String, u64, String)>, bool) {
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .arg("grep")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    let mut values = lines(&out);
    let truncated = values.pop().unwrap()["truncated"].as_bool().unwrap();
    let matches = values
        .iter()
        .map(|v| {
            (
                v["path"].as_str().unwrap().to_owned(),
                v["line"].as_u64().unwrap(),
                v["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    (matches, truncated)
}

#[test]
fn grep_reports_lines_in_path_then_line_order() {
    let dir = tree(&[
        ("b.txt", "one\nneedle two\nthree\n"),
        ("a/x.txt", "needle first\r\nlast needle"),
        ("c.txt", "nothing\n"),
    ]);
    let (matches, truncated) = grep(dir.path(), &["--pattern", "needle", "--limit", "10"]);
    assert_eq!(
        matches,
        [
            ("a/x.txt".to_owned(), 1, "needle first".to_owned()),
            ("a/x.txt".to_owned(), 2, "last needle".to_owned()),
            ("b.txt".to_owned(), 2, "needle two".to_owned()),
        ]
    );
    assert!(!truncated);
}

#[test]
fn grep_filters_by_glob_and_case() {
    let dir = tree(&[
        ("a.rs", "Needle\n"),
        ("b.txt", "Needle\n"),
        ("src/c.rs", "needle\n"),
    ]);
    let (matches, _) = grep(
        dir.path(),
        &[
            "--pattern",
            "needle",
            "--glob",
            "**/*.rs",
            "--ignore-case",
            "--limit",
            "10",
        ],
    );
    let paths: Vec<_> = matches.iter().map(|m| m.0.as_str()).collect();
    assert_eq!(paths, ["a.rs", "src/c.rs"]);
    let (matches, _) = grep(dir.path(), &["--pattern", "needle", "--limit", "10"]);
    assert_eq!(matches.len(), 1);
}

#[test]
fn grep_skips_binary_files_and_ignored_files() {
    let dir = tree(&[
        (".gitignore", "ignored.txt\n"),
        ("ignored.txt", "needle\n"),
        (".git/HEAD", "needle\n"),
        ("text.txt", "needle\n"),
    ]);
    fs::write(dir.path().join("blob.bin"), b"needle\0more").unwrap();
    let (matches, _) = grep(dir.path(), &["--pattern", "needle", "--limit", "10"]);
    let paths: Vec<_> = matches.iter().map(|m| m.0.as_str()).collect();
    assert_eq!(paths, ["text.txt"]);
}

#[test]
fn grep_truncates_at_the_limit() {
    let dir = tree(&[("a.txt", "x\nx\nx\n")]);
    let (matches, truncated) = grep(dir.path(), &["--pattern", "x", "--limit", "2"]);
    assert_eq!(matches.len(), 2);
    assert!(truncated);
    let (matches, truncated) = grep(dir.path(), &["--pattern", "x", "--limit", "3"]);
    assert_eq!(matches.len(), 3);
    assert!(!truncated);
}

#[test]
fn grep_searches_a_single_file_under_its_own_name() {
    let dir = tree(&[("sub/a.txt", "needle\n"), ("b.txt", "needle\n")]);
    let (matches, _) = grep(
        dir.path(),
        &[
            "--pattern",
            "needle",
            "--path",
            "sub/a.txt",
            "--limit",
            "10",
        ],
    );
    assert_eq!(matches, [("a.txt".to_owned(), 1, "needle".to_owned())]);
}

#[test]
fn grep_rejects_a_bad_regex() {
    let dir = tree(&[("a.txt", "x\n")]);
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args(["grep", "--pattern", "(", "--limit", "1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid regular expression"));
}

#[test]
fn read_prints_the_size_then_up_to_max_bytes() {
    let dir = tree(&[("f.txt", "0123456789")]);
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args(["read", "--max", "4", "f.txt"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"{\"size\":10}\n0123");
    let out = helper(run_dir.path())
        .args(["read", "f.txt"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"{\"size\":10}\n0123456789");
}

#[test]
fn read_fails_for_a_missing_file_and_a_directory() {
    let dir = tree(&[("d/f", "")]);
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args(["read", "missing"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(66));
    let out = helper(run_dir.path())
        .args(["read", "d"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is a directory"));
}

fn write(dir: &Path, path: &str, content: &[u8]) {
    let run_dir = TempDir::new().unwrap();
    let mut child = helper(run_dir.path())
        .args(["write", path])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(content).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn write_creates_parents_and_keeps_an_existing_files_mode() {
    let dir = tree(&[("script.sh", "old old old")]);
    let script = dir.path().join("script.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    write(dir.path(), "script.sh", b"new");
    assert_eq!(fs::read(&script).unwrap(), b"new");
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o755
    );

    write(dir.path(), "deep/er/file.txt", b"\x00binary\xff");
    assert_eq!(
        fs::read(dir.path().join("deep/er/file.txt")).unwrap(),
        b"\x00binary\xff"
    );
}

fn run_command(run_dir: &Path, script: &str) -> Command {
    let mut command = helper(run_dir);
    command.args(["run", "--id", "test-1", "--", "/bin/sh", "-c", script]);
    command
}

#[test]
fn run_forwards_output_and_the_exit_status() {
    let run_dir = TempDir::new().unwrap();
    let out = run_command(run_dir.path(), "echo out; echo err >&2; exit 3")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
}

#[test]
fn run_reports_a_signal_as_128_plus_n() {
    let run_dir = TempDir::new().unwrap();
    let out = run_command(run_dir.path(), "kill -9 $$").output().unwrap();
    assert_eq!(out.status.code(), Some(137));
}

#[test]
fn run_reports_a_missing_program_as_127() {
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args(["run", "--id", "x", "--", "/nonexistent/program"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn run_does_not_wait_for_background_processes_holding_its_output() {
    let run_dir = TempDir::new().unwrap();
    let started = Instant::now();
    let out = run_command(run_dir.path(), "sleep 20 & echo done")
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"done\n");
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn run_records_its_group_and_removes_the_record_when_done() {
    let run_dir = TempDir::new().unwrap();
    let record = run_dir.path().join("test-1");
    let mut child = run_command(run_dir.path(), "sleep 0.5").spawn().unwrap();
    wait_for(&record);
    assert!(child.wait().unwrap().success());
    assert!(!record.exists());
}

#[test]
fn kill_ends_the_command_and_its_descendants() {
    let run_dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let pid_file = work.path().join("child.pid");
    let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
    let mut child = run_command(run_dir.path(), &script).spawn().unwrap();
    wait_for(&pid_file);
    // Give the shell time to write the pid completely.
    std::thread::sleep(Duration::from_millis(100));
    let descendant = fs::read_to_string(&pid_file).unwrap().trim().to_owned();

    let killed = helper(run_dir.path())
        .args(["kill", "--id", "test-1"])
        .status()
        .unwrap();
    assert!(killed.success());
    assert_eq!(child.wait().unwrap().code(), Some(137));
    // The descendant is gone (`kill -0` fails for a process that doesn't exist).
    let alive = Command::new("kill")
        .args(["-0", &descendant])
        .status()
        .unwrap();
    assert!(!alive.success());
}

#[test]
fn kill_before_run_starts_makes_run_kill_the_command_at_once() {
    let run_dir = TempDir::new().unwrap();
    let killed = helper(run_dir.path())
        .args(["kill", "--id", "test-1"])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = Instant::now();
    let out = run_command(run_dir.path(), "sleep 30").output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(137));
}

#[test]
fn kill_rejects_an_id_that_is_not_a_file_name() {
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .args(["kill", "--id", "../etc"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn df_reports_free_bytes() {
    let dir = TempDir::new().unwrap();
    let run_dir = TempDir::new().unwrap();
    let out = helper(run_dir.path())
        .arg("df")
        .arg(dir.path())
        .output()
        .unwrap();
    let values = lines(&out);
    assert!(values[0]["free_bytes"].as_u64().unwrap() > 0);
    let out = helper(run_dir.path())
        .args(["df", "/nonexistent-dir"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(66));
}
