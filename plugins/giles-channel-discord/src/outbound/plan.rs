//! What a send posts: the rendered text and the files, as the messages that carry them, and the
//! limits that decide it.
//!
//! The limits are Discord's, from its documentation
//! (<https://discord.com/developers/docs/reference#uploading-files> and the Create Message
//! resource, checked 2026-10-26): **20 MiB per file** ("the default limit for all users", which a
//! bot in a DM has: Nitro and a server's boost tier are what raise it) and **25 MiB per request**.
//! The documentation states no limit on the number of files in a message. 10 is the figure
//! Discord's own clients and libraries use, so no more than that go in one message and the rest in
//! follow-up messages; real Discord confirms it (issue #20).

use std::hash::Hash;

use giles_plugin_sdk::giles_types::{Error, MediaRef, Result};

/// The largest file Discord takes from a bot in a DM, in bytes.
pub const FILE_LIMIT: u64 = 20 * 1024 * 1024;
/// The largest request Discord takes, in bytes: the files of one message, and the text.
pub const REQUEST_LIMIT: u64 = 25 * 1024 * 1024;
/// What of [`REQUEST_LIMIT`] is kept for everything that isn't file content: the text (2000
/// characters of up to four bytes), the JSON around it, and the multipart headers.
pub const REQUEST_OVERHEAD: u64 = 64 * 1024;
/// The most files in one message. More are sent in follow-up messages.
pub const FILES_PER_MESSAGE: usize = 10;

/// One message to post: its text, which may be empty, and the files it carries. The files are
/// read from the blob store only when the message is about to be posted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Post {
    /// The text, one part of the rendering.
    pub content: String,
    /// The files, at most [`FILES_PER_MESSAGE`], with the content of a request fitting
    /// [`REQUEST_LIMIT`].
    pub files: Vec<MediaRef>,
}

/// The most bytes one file may have: Discord's limit, or `max_blob` if that is less.
pub fn file_limit(max_blob: u64) -> u64 {
    max_blob.min(FILE_LIMIT)
}

/// Refuses a file over [`file_limit`], by the size the core gave it, before anything is sent.
///
/// # Errors
///
/// `InvalidRequest` naming the file, its size, and the limit: a retry can't make it smaller.
pub fn check(files: &[MediaRef], max_blob: u64) -> Result<()> {
    let limit = file_limit(max_blob);
    match files.iter().find(|file| file.size > limit) {
        None => Ok(()),
        Some(file) => Err(Error::InvalidRequest(format!(
            "attachment {:?} is {} bytes, over the limit of {limit} bytes ({})",
            file.name.as_deref().unwrap_or(&file.media_type),
            file.size,
            if limit == max_blob {
                "max_blob"
            } else {
                "what Discord takes from a bot in a DM"
            },
        ))),
    }
}

/// The messages that carry `parts` (the rendered text, in order) and `files`, in the order they
/// are posted.
///
/// - Text that fits one message is that message's content, and the first message of files is it:
///   the file goes with its words.
/// - Longer text goes first, one message per part, and the files follow with no text.
/// - Files go [`FILES_PER_MESSAGE`] to a message, and a message takes files only while they fit
///   [`REQUEST_LIMIT`] (so two files of 20 MiB are two messages); each file is within
///   [`FILE_LIMIT`], so it always fits alone. Order is kept.
pub fn plan(parts: Vec<String>, files: Vec<MediaRef>) -> Vec<Post> {
    let mut groups = group(files).into_iter();
    if parts.len() == 1 {
        let content = parts.into_iter().next().unwrap_or_default();
        let first = groups.next().unwrap_or_default();
        return std::iter::once(Post {
            content,
            files: first,
        })
        .chain(groups.map(|files| Post {
            content: String::new(),
            files,
        }))
        .collect();
    }
    parts
        .into_iter()
        .map(|content| Post {
            content,
            files: Vec::new(),
        })
        .chain(groups.map(|files| Post {
            content: String::new(),
            files,
        }))
        .collect()
}

/// `files` in groups that fit a message, in order.
fn group(files: Vec<MediaRef>) -> Vec<Vec<MediaRef>> {
    let budget = REQUEST_LIMIT - REQUEST_OVERHEAD;
    let mut groups: Vec<Vec<MediaRef>> = Vec::new();
    let mut size = 0_u64;
    for file in files {
        match groups.last_mut() {
            Some(group) if group.len() < FILES_PER_MESSAGE && size + file.size <= budget => {
                size += file.size;
                group.push(file);
            }
            _ => {
                size = file.size;
                groups.push(vec![file]);
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use giles_plugin_sdk::giles_types::MediaId;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn file(n: usize, size: u64) -> MediaRef {
        MediaRef {
            id: MediaId::new(format!("{n:064x}")).unwrap(),
            media_type: "image/png".to_owned(),
            size,
            name: Some(format!("file{n}.png")),
        }
    }

    fn shape(posts: &[Post]) -> Vec<(&str, Vec<usize>)> {
        posts
            .iter()
            .map(|post| {
                (
                    post.content.as_str(),
                    post.files
                        .iter()
                        .map(|f| f.name.as_deref().unwrap()[4..5].parse().unwrap())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn short_text_is_the_body_of_the_first_message_of_files() {
        let posts = plan(vec!["look".to_owned()], vec![file(1, 10), file(2, 10)]);
        assert_eq!(shape(&posts), [("look", vec![1, 2])]);
    }

    #[test]
    fn long_text_goes_first_and_the_files_follow_without_it() {
        let parts = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
        let posts = plan(parts, vec![file(1, 10)]);
        assert_eq!(
            shape(&posts),
            [
                ("one", vec![]),
                ("two", vec![]),
                ("three", vec![]),
                ("", vec![1])
            ]
        );
    }

    #[test]
    fn files_alone_are_messages_with_no_text_and_text_alone_has_no_files() {
        assert_eq!(shape(&plan(vec![], vec![file(1, 10)])), [("", vec![1])]);
        assert_eq!(
            shape(&plan(vec!["a".to_owned(), "b".to_owned()], vec![])),
            [("a", vec![]), ("b", vec![])]
        );
        assert_eq!(shape(&plan(vec!["a".to_owned()], vec![])), [("a", vec![])]);
        assert!(plan(vec![], vec![]).is_empty());
    }

    #[test]
    fn ten_files_fit_a_message_and_the_eleventh_follows() {
        let files: Vec<_> = (0..11).map(|n| file(n, 1)).collect();
        let posts = plan(vec!["x".to_owned()], files);
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].files.len(), FILES_PER_MESSAGE);
        assert_eq!(posts[0].content, "x");
        assert_eq!(posts[1].files.len(), 1);
        assert_eq!(posts[1].content, "");
        // Order is kept across the messages.
        let names: Vec<_> = posts
            .iter()
            .flat_map(|post| post.files.iter().map(|f| f.name.clone().unwrap()))
            .collect();
        assert_eq!(
            names,
            (0..11).map(|n| format!("file{n}.png")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_message_takes_files_only_while_they_fit_the_request() {
        // 20 + 4 MiB fit; the next 20 MiB would not.
        let files = vec![file(1, 20 * MIB), file(2, 4 * MIB), file(3, 20 * MIB)];
        let posts = plan(vec![], files);
        assert_eq!(shape(&posts), [("", vec![1, 2]), ("", vec![3])]);
        // The overhead is kept back: 20 + 5 MiB would be exactly the limit, with nothing left
        // for the text and the headers.
        let posts = plan(vec![], vec![file(1, 20 * MIB), file(2, 5 * MIB)]);
        assert_eq!(posts.len(), 2);
        // A file of the limit fits alone, whatever came before it.
        let posts = plan(vec![], vec![file(1, 1), file(2, FILE_LIMIT)]);
        assert_eq!(shape(&posts), [("", vec![1, 2])]);
    }

    #[test]
    fn an_oversize_file_is_refused_by_its_declared_size() {
        let ok = [file(1, FILE_LIMIT), file(2, 0)];
        check(&ok, 50 * MIB).unwrap();
        let error = check(&[file(1, 1), file(2, FILE_LIMIT + 1)], 50 * MIB).unwrap_err();
        let Error::InvalidRequest(text) = error else {
            panic!("not InvalidRequest: {error:?}");
        };
        assert!(text.contains("file2.png"), "{text}");
        assert!(text.contains(&(FILE_LIMIT + 1).to_string()), "{text}");
        assert!(text.contains("in a DM"), "{text}");
    }

    #[test]
    fn max_blob_caps_the_limit_below_discords_and_says_so() {
        assert_eq!(file_limit(50 * MIB), FILE_LIMIT);
        assert_eq!(file_limit(5 * MIB), 5 * MIB);
        check(&[file(1, 5 * MIB)], 5 * MIB).unwrap();
        let Error::InvalidRequest(text) = check(&[file(1, 5 * MIB + 1)], 5 * MIB).unwrap_err()
        else {
            panic!("not InvalidRequest");
        };
        assert!(text.contains("max_blob"), "{text}");
    }
}
