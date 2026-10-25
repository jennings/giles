//! Agent management for the web UI (PLAN sections 8.7, 11.2, 11.3): what an
//! agent's page shows ([`files`]) and creating ([`create`]), editing ([`edit`]), and removing
//! ([`remove`]) an agent while the core runs. The web
//! channel's agent pages are the only client so far; nothing here depends on HTTP.

pub mod create;
pub mod edit;
pub mod files;
mod manager;
pub mod remove;

pub use create::{
    CreateError, DEFAULT_SOUL, MAX_SOUL_BYTES, NewAgent, RESERVED_AGENT_IDS, agent_id_from_name,
    agent_toml, split_list,
};
pub use edit::{AgentEdit, AgentForm, EditError, MAX_EDIT_FILE_BYTES, edited_toml};
pub use files::{
    AgentFile, AgentFiles, Content, FileKind, MAX_DEPTH, MAX_FILE_BYTES, MAX_FILES,
    MAX_TOTAL_BYTES, list_files, percent_decode,
};
pub use manager::{AgentManager, AgentManagerOptions};
pub use remove::{AgentActivity, DaemonActivity, RemovalPreview, RemoveError, Removed, RouteRef};
