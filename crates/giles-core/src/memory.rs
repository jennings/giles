//! What the memory tools and the prompt share about agent memory (PLAN sections 6.7 and 11.3).
//!
//! The memories themselves live in the agent's memory database
//! ([`giles_store::MemoryStore`], opened through [`crate::memory_dbs::MemoryDbs`]); the store
//! decides what a [`MemoryViewer`] sees. This module decides who the viewer is:
//!
//! - [`viewer`]: for a tool call, from the turn's session and its requester.
//! - [`prompt_viewer`]: for the prompt's memory snapshot, from the session alone.
//!
//! and how a memory is rendered the same way in tool results and the prompt ([`flags`],
//! [`line`]), plus the prompt's usage header ([`usage_header`]) and the location of the user's
//! profile file ([`user_profile_file`]).
//!
//! # Scopes
//!
//! - A direct session (DM) or a job session belongs to one user, so its private scope is that
//!   user's: [`MemoryOwner::User`]. A runtime job runs as its creator, in the creator's DM or a
//!   job session owned by the creator, so it sees the creator's memories.
//! - A group session's private scope is the group conversation: [`MemoryOwner::Group`].
//! - A config job has no requester. It runs in a session owned by the owner (their DM or a job
//!   session), but with no requester there is nobody whose private memories to use, so its scope
//!   is [`MemoryOwner::None`]: shared memories only.
//!
//! Shared memories are writable by the agent owner's turns and by config jobs. The size limit is
//! the agent's `memory_cap` ([`AgentConfig::memory_cap`](crate::config::AgentConfig),
//! default [`DEFAULT_MEMORY_CAP`](crate::config::DEFAULT_MEMORY_CAP)), in characters, per scope,
//! on core memories only; the callers pass it to the store as `core_cap`.

use std::path::PathBuf;

use giles_store::{Memory, MemoryOwner, MemoryViewer, Session, SessionKind};
use giles_types::UserId;

use crate::identity;
use crate::paths::{DataRoot, PathsError};

/// `users/<user>/USER.md`: the user's profile. People write it; the model only reads it.
///
/// # Errors
///
/// [`PathsError::InvalidName`] if `user` is not a valid name.
pub fn user_profile_file(root: &DataRoot, user: &str) -> Result<PathBuf, PathsError> {
    Ok(root.user_dir(user)?.join("USER.md"))
}

/// The memory viewer for a tool call in `session`, made by `requester` (`None`: a config job's
/// run, which has no requester).
///
/// - The private scope is the session's: the session's user in a direct or job session, the
///   conversation in a group session. A turn without a requester (a config job) gets
///   [`MemoryOwner::None`] instead, so it sees and writes shared memories only. A session that
///   has no user (or no conversation, for a group) gives [`MemoryOwner::None`] too.
/// - [`MemoryViewer::can_write_shared`] is true for a config job and for the agent owner's turns.
///
/// A runtime job's run has its creator as requester, so it is treated as that user's turn.
///
/// # Errors
///
/// The message for the model if `requester` may not use the session's private memories: in a
/// direct or job session, anyone but the session's user. Any participant may use a group's.
pub fn viewer(session: &Session, requester: Option<&UserId>) -> Result<MemoryViewer, String> {
    let Some(requester) = requester else {
        return Ok(MemoryViewer::new(MemoryOwner::None, true));
    };
    let owner = session_owner(session);
    if let (MemoryOwner::User(user), true) = (&owner, Some(requester) != session_user(session)) {
        return Err(format!(
            "not permitted for {requester}: this conversation's private memories belong to {user}"
        ));
    }
    Ok(MemoryViewer::new(owner, *requester == identity::owner()))
}

/// The viewer the prompt's memory snapshot uses for `session`: no requester is known when the
/// prompt is built, so it assumes the session's own user.
///
/// - The private scope is the one [`viewer`] gives that user: [`MemoryOwner::User`] in a direct
///   or job session, [`MemoryOwner::Group`] in a group session.
/// - [`MemoryViewer::can_write_shared`] is true iff the session is a direct or job session of
///   the agent owner. In a group session who may write shared memories depends on who speaks, so
///   the prompt treats shared memories there as not writable.
///
/// The prompt can't tell a config job's session from a runtime job's: both are the user's DM or a
/// job session owned by them. So a config job's prompt shows the owner's private core memories,
/// which its tools (viewer [`MemoryOwner::None`], see [`viewer`]) can neither read nor write; the
/// tools stay authoritative, and the job sees only shared memories through `memory_recall`.
pub fn prompt_viewer(session: &Session) -> MemoryViewer {
    let can_write_shared = session.kind != SessionKind::Group
        && session_user(session).is_some_and(|user| *user == identity::owner());
    MemoryViewer::new(session_owner(session), can_write_shared)
}

/// The private scope of `session`'s own participants.
fn session_owner(session: &Session) -> MemoryOwner {
    match session.kind {
        SessionKind::Group => match (&session.channel, &session.conversation) {
            (Some(instance), Some(conversation)) => MemoryOwner::Group {
                instance: instance.clone(),
                conversation: conversation.clone(),
            },
            _ => MemoryOwner::None,
        },
        _ => session
            .user
            .clone()
            .map_or(MemoryOwner::None, MemoryOwner::User),
    }
}

/// The user a direct or job session belongs to; `None` in a group session.
fn session_user(session: &Session) -> Option<&UserId> {
    match session.kind {
        SessionKind::Group => None,
        _ => session.user.as_ref(),
    }
}

/// A memory's flags as the model sees them: `core, shared`, `core`, `shared`, or `private`.
pub fn flags(memory: &Memory) -> String {
    match (memory.core, memory.shared) {
        (true, true) => "core, shared",
        (true, false) => "core",
        (false, true) => "shared",
        (false, false) => "private",
    }
    .to_owned()
}

/// One memory as tool results and the prompt list it: `#12 (core, shared): content`.
pub fn line(memory: &Memory) -> String {
    format!("#{} ({}): {}", memory.id, flags(memory), memory.content)
}

/// The usage header shown above a core memory scope in the prompt: `[67% — 1,474/2,200]`.
///
/// The percentage is rounded down, so 100% means at the cap; an over-cap scope shows more than
/// 100%. With a cap of 0, empty memory is 0% and anything else 100%.
pub fn usage_header(used: usize, cap: usize) -> String {
    let percent = match cap {
        0 if used == 0 => 0,
        0 => 100,
        cap => used.saturating_mul(100) / cap,
    };
    format!(
        "[{percent}% — {}/{}]",
        group_digits(used),
        group_digits(cap)
    )
}

/// `n` with commas between groups of three digits: `1,474`.
fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use giles_store::{MemoryId, SessionId};
    use giles_types::{AgentId, ConversationId, Timestamp};

    use super::*;

    fn user(name: &str) -> UserId {
        UserId::new(name).unwrap()
    }

    fn session(kind: SessionKind, user: Option<UserId>) -> Session {
        let channel = (kind != SessionKind::Job).then(|| "fake".to_owned());
        Session {
            id: SessionId::new(1),
            agent: AgentId::new("jeeves").unwrap(),
            kind,
            conversation: channel
                .as_ref()
                .map(|_| ConversationId::new("chat").unwrap()),
            channel,
            user,
            started_at: Timestamp::UNIX_EPOCH,
            ended_at: None,
            last_message_at: None,
        }
    }

    fn group_owner() -> MemoryOwner {
        MemoryOwner::Group {
            instance: "fake".to_owned(),
            conversation: ConversationId::new("chat").unwrap(),
        }
    }

    fn memory(core: bool, shared: bool) -> Memory {
        Memory {
            id: MemoryId::new(12),
            content: "Likes tea.".to_owned(),
            owner: MemoryOwner::User(identity::owner()),
            shared,
            core,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            source_session: None,
        }
    }

    #[test]
    fn viewer_in_the_owners_dm_writes_shared_and_sees_their_memories() {
        let dm = session(SessionKind::Direct, Some(identity::owner()));
        let viewer = viewer(&dm, Some(&identity::owner())).unwrap();
        assert_eq!(
            viewer,
            MemoryViewer::new(MemoryOwner::User(identity::owner()), true)
        );
    }

    #[test]
    fn viewer_in_a_guests_dm_cannot_write_shared() {
        let guest = user("guest");
        let dm = session(SessionKind::Direct, Some(guest.clone()));
        let viewer = viewer(&dm, Some(&guest)).unwrap();
        assert_eq!(viewer, MemoryViewer::new(MemoryOwner::User(guest), false));
    }

    #[test]
    fn viewer_in_a_job_session_of_its_creator_is_that_users() {
        let job = session(SessionKind::Job, Some(identity::owner()));
        let viewer = viewer(&job, Some(&identity::owner())).unwrap();
        assert_eq!(
            viewer,
            MemoryViewer::new(MemoryOwner::User(identity::owner()), true)
        );
    }

    #[test]
    fn viewer_in_a_group_is_the_conversation_for_any_participant() {
        let group = session(SessionKind::Group, None);
        let guest = user("guest");
        assert_eq!(
            viewer(&group, Some(&guest)).unwrap(),
            MemoryViewer::new(group_owner(), false)
        );
        assert_eq!(
            viewer(&group, Some(&identity::owner())).unwrap(),
            MemoryViewer::new(group_owner(), true)
        );
    }

    #[test]
    fn viewer_of_a_config_job_sees_shared_memories_only_and_writes_them() {
        let expected = MemoryViewer::new(MemoryOwner::None, true);
        for session in [
            session(SessionKind::Direct, Some(identity::owner())),
            session(SessionKind::Job, Some(identity::owner())),
        ] {
            assert_eq!(viewer(&session, None).unwrap(), expected);
        }
    }

    #[test]
    fn viewer_refuses_another_user_in_a_dm_or_job_session() {
        let guest = user("guest");
        for kind in [SessionKind::Direct, SessionKind::Job] {
            let session = session(kind, Some(identity::owner()));
            let error = viewer(&session, Some(&guest)).unwrap_err();
            assert!(error.contains("not permitted for guest"), "{error}");
            assert!(error.contains(identity::OWNER), "{error}");
        }
    }

    #[test]
    fn viewer_of_a_session_without_a_user_has_no_private_scope() {
        let dm = session(SessionKind::Direct, None);
        assert_eq!(
            viewer(&dm, Some(&identity::owner())).unwrap(),
            MemoryViewer::new(MemoryOwner::None, true)
        );
    }

    #[test]
    fn prompt_viewer_for_each_session_kind() {
        let guest = user("guest");
        assert_eq!(
            prompt_viewer(&session(SessionKind::Direct, Some(identity::owner()))),
            MemoryViewer::new(MemoryOwner::User(identity::owner()), true)
        );
        assert_eq!(
            prompt_viewer(&session(SessionKind::Direct, Some(guest.clone()))),
            MemoryViewer::new(MemoryOwner::User(guest.clone()), false)
        );
        assert_eq!(
            prompt_viewer(&session(SessionKind::Job, Some(identity::owner()))),
            MemoryViewer::new(MemoryOwner::User(identity::owner()), true)
        );
        assert_eq!(
            prompt_viewer(&session(SessionKind::Job, Some(guest.clone()))),
            MemoryViewer::new(MemoryOwner::User(guest), false)
        );
        // Who may write shared memories in a group depends on who speaks.
        assert_eq!(
            prompt_viewer(&session(SessionKind::Group, None)),
            MemoryViewer::new(group_owner(), false)
        );
        assert_eq!(
            prompt_viewer(&session(SessionKind::Direct, None)),
            MemoryViewer::new(MemoryOwner::None, false)
        );
    }

    #[test]
    fn prompt_viewer_matches_the_tools_viewer_for_the_sessions_own_user() {
        let guest = user("guest");
        let sessions = [
            (
                session(SessionKind::Direct, Some(identity::owner())),
                identity::owner(),
            ),
            (
                session(SessionKind::Job, Some(identity::owner())),
                identity::owner(),
            ),
            (
                session(SessionKind::Direct, Some(guest.clone())),
                guest.clone(),
            ),
            (session(SessionKind::Job, Some(guest.clone())), guest),
        ];
        for (session, requester) in sessions {
            assert_eq!(
                prompt_viewer(&session),
                viewer(&session, Some(&requester)).unwrap()
            );
        }
        let group = session(SessionKind::Group, None);
        assert_eq!(
            prompt_viewer(&group).owner,
            viewer(&group, Some(&user("guest"))).unwrap().owner
        );
    }

    #[test]
    fn flags_and_lines() {
        let cases = [
            ((true, true), "core, shared"),
            ((true, false), "core"),
            ((false, true), "shared"),
            ((false, false), "private"),
        ];
        for ((core, shared), expected) in cases {
            let memory = memory(core, shared);
            assert_eq!(flags(&memory), expected);
            assert_eq!(line(&memory), format!("#12 ({expected}): Likes tea."));
        }
    }

    #[test]
    fn usage_headers() {
        let cases = [
            ((1474, 2200), "[67% — 1,474/2,200]"),
            ((0, 2200), "[0% — 0/2,200]"),
            ((2199, 2200), "[99% — 2,199/2,200]"),
            ((2200, 2200), "[100% — 2,200/2,200]"),
            ((2300, 2200), "[104% — 2,300/2,200]"),
            ((5, 999), "[0% — 5/999]"),
            ((1_234_567, 1_000_000), "[123% — 1,234,567/1,000,000]"),
            ((0, 0), "[0% — 0/0]"),
            ((3, 0), "[100% — 3/0]"),
        ];
        for ((used, cap), header) in cases {
            assert_eq!(usage_header(used, cap), header);
        }
    }

    #[test]
    fn user_profile_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(tmp.path(), tmp.path()).unwrap();
        assert_eq!(
            user_profile_file(&root, "stephen").unwrap(),
            root.users_dir().join("stephen/USER.md")
        );
        assert!(user_profile_file(&root, "a/b").is_err());
    }
}
