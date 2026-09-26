//! Tools that run inside SCV itself: reading and writing workspace files,
//! loading skills, the shell, the web, sending a file back to a chat, and
//! the chat's own history.

pub mod chat_attach;
pub mod chat_history;
pub(crate) mod fs;
pub(crate) mod shell;
pub(crate) mod skill;
pub mod web;
