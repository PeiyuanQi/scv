//! The `agent` tool, which hands a turn to another agent.
//!
//! [`agent`] is the tool the model calls, with the agent as an argument. It
//! passes the call to that agent's backend, which runs one of three ways:
//! [`native`] starts its CLI once per turn, [`acp`] keeps an Agent Client
//! Protocol server per conversation, and [`scv`] keeps a nested
//! `scv server --stdio`. The rest is shared:
//!
//! - [`adapters`]: the built-in agents and how to launch each one.
//! - [`request`]: the arguments every agent call takes.
//! - [`choice`]: what the model is told to choose between agents.
//! - [`background`]: jobs that run while the conversation goes on.
//! - [`conversation`]: handles that continue an agent's conversation.
//! - [`records`]: the on-disk record of every delegated process, for
//!   listing, stopping, and cleanup.
//! - [`stores`]: the credential files each agent CLI reads, in its own
//!   format, inside SCV's private agent homes.
//! - [`live`]: a long-lived child process read line by line.
//! - [`output`] and [`progress`]: reading a run's output into one bounded
//!   result, and its progress into short lines.

pub(crate) mod acp;
pub mod adapters;
pub(crate) mod agent;
pub mod background;
pub mod choice;
pub mod conversation;
pub(crate) mod live;
pub(crate) mod native;
pub(crate) mod output;
pub(crate) mod progress;
pub mod records;
pub(crate) mod request;
pub(crate) mod scv;
pub mod stores;
