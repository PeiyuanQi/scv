//! Tools that run inside SCV itself: reading and writing workspace files,
//! loading skills, the shell, the web, and sending a file back to a chat.

pub mod chat_attach;
pub(crate) mod fs;
pub(crate) mod shell;
pub(crate) mod skill;
pub mod web;
