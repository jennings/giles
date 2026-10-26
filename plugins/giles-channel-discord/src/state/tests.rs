//! The state file: the format, the atomic write, and what happens to a file that isn't usable.

use std::fs;

use super::{DmChannel, FILE_NAME, StateFile, corrupt_path};

const CHANNEL: u64 = 319_674_150_115_610_528;
const USER: u64 = 82_198_898_841_029_460;
const MESSAGE: u64 = 1_234_567_890_123_456_789;

fn read(dir: &tempfile::TempDir) -> serde_json::Value {
    serde_json::from_slice(&fs::read(dir.path().join(FILE_NAME)).unwrap()).unwrap()
}

#[test]
fn a_missing_file_is_an_empty_state_and_nothing_is_written_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    assert_eq!(state.channels(), Vec::<u64>::new());
    assert_eq!(state.get(CHANNEL), None);
    assert_eq!(state.channel_for_user(USER), None);
    assert!(!dir.path().join(FILE_NAME).exists());
}

#[test]
fn the_file_has_the_documented_shape_with_ids_as_strings() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    state.record_dm(CHANNEL, USER);
    state.advance(CHANNEL, MESSAGE);
    assert_eq!(
        read(&dir),
        serde_json::json!({
            "version": 1,
            "dm_channels": {
                "319674150115610528": {
                    "cursor": "1234567890123456789",
                    "user": "82198898841029460"
                }
            }
        })
    );
}

#[test]
fn what_was_written_is_read_back_by_a_new_process() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    state.record_dm(CHANNEL, USER);
    state.advance(CHANNEL, MESSAGE);
    // A channel known only by its cursor, and one only by its user, as a send creates it.
    state.advance(7, 70);
    state.record_dm(8, 80);

    let reopened = StateFile::open(dir.path());
    assert_eq!(reopened.channels(), [7, 8, CHANNEL]);
    assert_eq!(
        reopened.get(CHANNEL),
        Some(DmChannel {
            cursor: Some(MESSAGE),
            user: Some(USER)
        })
    );
    assert_eq!(
        reopened.get(7),
        Some(DmChannel {
            cursor: Some(70),
            user: None
        })
    );
    assert_eq!(
        reopened.get(8),
        Some(DmChannel {
            cursor: None,
            user: Some(80)
        })
    );
    assert_eq!(reopened.channel_for_user(USER), Some(CHANNEL));
    assert_eq!(reopened.channel_for_user(80), Some(8));
    assert_eq!(reopened.channel_for_user(999), None);
}

#[test]
fn the_cursor_only_moves_forward_and_a_user_does_not_move_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    state.advance(CHANNEL, 200);
    state.advance(CHANNEL, 100);
    state.advance(CHANNEL, 200);
    assert_eq!(state.get(CHANNEL).unwrap().cursor, Some(200));
    state.record_dm(CHANNEL, USER);
    assert_eq!(state.get(CHANNEL).unwrap().cursor, Some(200));
    state.advance(CHANNEL, 300);
    assert_eq!(state.get(CHANNEL).unwrap().cursor, Some(300));
    assert_eq!(state.get(CHANNEL).unwrap().user, Some(USER));
}

#[test]
fn a_write_replaces_the_file_whole_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    for message in 1..=20 {
        state.advance(CHANNEL, message);
        // At every point the file on disk parses and is the state as of the last call.
        assert_eq!(
            read(&dir)["dm_channels"][CHANNEL.to_string()]["cursor"],
            message.to_string()
        );
    }
    let mut names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [FILE_NAME], "no temporary file is left");
}

#[test]
fn a_change_that_changes_nothing_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::open(dir.path());
    state.advance(CHANNEL, 10);
    // Replace the file; an unchanged state must not overwrite it.
    fs::write(dir.path().join(FILE_NAME), "marker").unwrap();
    state.advance(CHANNEL, 10);
    state.advance(CHANNEL, 5);
    assert_eq!(
        fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
        "marker"
    );
    state.record_dm(CHANNEL, USER);
    state.record_dm(CHANNEL, USER);
    assert_ne!(
        fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
        "marker"
    );
}

/// A file that can't be used is moved aside, the plugin starts empty, and the next write makes a
/// good file.
fn assert_set_aside(contents: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE_NAME);
    fs::write(&path, contents).unwrap();

    let state = StateFile::open(dir.path());
    assert_eq!(state.channels(), Vec::<u64>::new(), "{contents:?}");
    assert!(!path.exists(), "{contents:?}");
    assert_eq!(fs::read(corrupt_path(&path)).unwrap(), contents);

    state.advance(CHANNEL, MESSAGE);
    assert_eq!(
        StateFile::open(dir.path()).get(CHANNEL).unwrap().cursor,
        Some(MESSAGE)
    );
    assert_eq!(fs::read(corrupt_path(&path)).unwrap(), contents, "kept");
}

#[test]
fn a_corrupt_file_is_set_aside_not_deleted() {
    assert_set_aside(b"");
    assert_set_aside(b"{\"version\": 1, \"dm_channels\": {");
    assert_set_aside(b"not json at all \xff\xfe");
    // Valid JSON of the wrong shape, a version from the future, and an id that isn't a number.
    assert_set_aside(br#"["version", 1]"#);
    assert_set_aside(br#"{"version": 2, "dm_channels": {}}"#);
    assert_set_aside(br#"{"dm_channels": {}}"#);
    assert_set_aside(br#"{"version": 1, "dm_channels": {"abc": {}}}"#);
    assert_set_aside(br#"{"version": 1, "dm_channels": {"1": {"cursor": "12x"}}}"#);
    assert_set_aside(br#"{"version": 1, "dm_channels": {"1": {"cursor": 12}}}"#);
}

#[test]
fn a_second_corrupt_file_replaces_the_first_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE_NAME);
    fs::write(&path, "first").unwrap();
    let _ = StateFile::open(dir.path());
    fs::write(&path, "second").unwrap();
    let _ = StateFile::open(dir.path());
    assert_eq!(fs::read_to_string(corrupt_path(&path)).unwrap(), "second");
}

#[test]
fn a_file_with_no_channels_and_unknown_fields_in_a_channel_is_read() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(FILE_NAME),
        r#"{"version": 1, "dm_channels": {"5": {"cursor": "50", "later": true}, "6": {}}}"#,
    )
    .unwrap();
    let state = StateFile::open(dir.path());
    assert_eq!(state.channels(), [5, 6]);
    assert_eq!(state.get(5).unwrap().cursor, Some(50));
    assert_eq!(state.get(6), Some(DmChannel::default()));
}

#[test]
fn a_failed_write_keeps_the_state_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    // The data directory is a file: nothing can be written under it.
    let file = dir.path().join("not-a-directory");
    fs::write(&file, "").unwrap();
    let state = StateFile::open(&file);
    state.advance(CHANNEL, MESSAGE);
    state.record_dm(CHANNEL, USER);
    assert_eq!(
        state.get(CHANNEL),
        Some(DmChannel {
            cursor: Some(MESSAGE),
            user: Some(USER)
        })
    );
}
