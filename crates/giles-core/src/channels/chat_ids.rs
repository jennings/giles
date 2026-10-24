//! Web chat ids (PLAN section 10): the conversation ids of the built-in web channel, outside the
//! `channel-web` feature so that code without the feature can read them.
//!
//! A chat id is 128 random bits as 32 lowercase hex digits (`0123…ef`).

use giles_types::ConversationId;

/// The web channel's component name, and the identity namespace of web senders (`web:<id>` in
/// `[owner]`).
pub const COMPONENT: &str = "web";

/// `bytes` random bytes from the OS, as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new chat id: 32 random lowercase hex digits. Web chats are created by the user
/// (`/chats/new`), and the chat id is the conversation id.
pub fn new_chat_id() -> ConversationId {
    ConversationId::new(random_hex(16)).expect("hex is a valid id")
}

/// Whether `id` has the form of a chat id ([`new_chat_id`]). Pages refuse anything else.
pub fn is_chat_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
