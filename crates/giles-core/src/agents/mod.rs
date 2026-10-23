//! Agent management for the web UI (TASKS.md 5.1; PLAN sections 8.7, 11.2, 11.3): what an agent's
//! page shows ([`files`]) and creating an agent while the core runs ([`create`]). The web
//! channel's agent pages are the only client so far; nothing here depends on HTTP.

pub mod create;
pub mod files;

pub use create::{
    AgentCreator, CreateError, MAX_SOUL_BYTES, NewAgent, RESERVED_AGENT_IDS, agent_toml, split_list,
};
pub use files::{
    AgentFile, AgentFiles, Content, FileKind, MAX_DEPTH, MAX_FILE_BYTES, MAX_FILES,
    MAX_TOTAL_BYTES, list_files, percent_decode,
};
