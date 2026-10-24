//! Web chat ids (PLAN section 10): the conversation ids of the built-in web channel. Outside the
//! `channel-web` feature because the router reads them: a chat started for a chosen agent goes to
//! that agent ([`Router`](crate::router::Router)).
//!
//! A chat id is 128 random bits as 32 lowercase hex digits (`0123…ef`), routed by the stored routes
//! like any conversation, or `<agent>.<32 hex digits>` for a chat started with an agent picked
//! in the web UI. Agent ids are `[a-zA-Z0-9-]+`, so the `.` is unambiguous and the whole id is
//! safe in a URL path.

use giles_types::{AgentId, ConversationId};

use crate::config::is_valid_name;

/// The web channel's component name, and the identity namespace of web senders (`web:<id>` in
/// `[owner]`).
pub const COMPONENT: &str = "web";

/// `bytes` random bytes from the OS, as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new chat id routed by the stored routes: 32 random lowercase hex digits. Web chats are created
/// by the user (`/chats/new`), and the chat id is the conversation id.
pub fn new_chat_id() -> ConversationId {
    ConversationId::new(random_hex(16)).expect("hex is a valid id")
}

/// A new chat id for a chat with `agent`: `<agent>.<32 random hex digits>`.
pub fn new_agent_chat_id(agent: &AgentId) -> ConversationId {
    ConversationId::new(format!("{agent}.{}", random_hex(16))).expect("a valid id")
}

/// Whether `id` has the form of a chat id ([`new_chat_id`] or [`new_agent_chat_id`]). Pages
/// refuse anything else.
pub fn is_chat_id(id: &str) -> bool {
    match id.split_once('.') {
        Some((agent, random)) => is_valid_name(agent) && is_random_part(random),
        None => is_random_part(id),
    }
}

/// The agent a chat was started with, if `id` is a chat id of the form `<agent>.<hex>`.
pub fn chat_agent(id: &str) -> Option<AgentId> {
    let (agent, random) = id.split_once('.')?;
    if !is_valid_name(agent) || !is_random_part(random) {
        return None;
    }
    AgentId::new(agent).ok()
}

fn is_random_part(part: &str) -> bool {
    part.len() == 32 && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_and_their_agents() {
        let plain = new_chat_id();
        assert!(is_chat_id(plain.as_str()));
        assert_eq!(chat_agent(plain.as_str()), None);
        assert_ne!(plain, new_chat_id());

        let bunter = AgentId::new("bunter-2").unwrap();
        let chosen = new_agent_chat_id(&bunter);
        assert!(chosen.as_str().starts_with("bunter-2."), "{chosen}");
        assert!(is_chat_id(chosen.as_str()));
        assert_eq!(chat_agent(chosen.as_str()), Some(bunter));

        let hex = "0123456789abcdef0123456789abcdef";
        for bad in [
            "owner",
            "",
            &hex[1..],
            &hex.to_ascii_uppercase(),
            &format!("{hex}0"),
            &format!(".{hex}"),
            &format!("a.b.{hex}"),
            &format!("../{hex}"),
            &format!("bunter_x.{hex}"),
            &format!("bunter.{}", &hex[1..]),
            &format!("{hex}.bunter"),
        ] {
            assert!(!is_chat_id(bad), "{bad}");
            assert_eq!(chat_agent(bad), None, "{bad}");
        }
    }
}
