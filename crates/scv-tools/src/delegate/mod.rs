//! The `agent` tool, which hands a turn to another agent.
//!
//! [`agent`] is the tool the model calls, with the agent as an argument. It
//! passes the call to that agent's backend, which runs one of three ways:
//! [`native`] starts its CLI once per turn, [`acp`] keeps an Agent Client
//! Protocol server per conversation, and [`scv`] keeps a nested
//! `scv server --stdio`. The rest is shared:
//!
//! - [`adapters`]: the built-in agents and how to launch each one.
//! - [`activity`]: each run's activity log and current state, which the
//!   workbench shows as the agent's live view.
//! - [`request`]: the arguments every agent call takes.
//! - [`choice`]: what the model is told to choose between agents.
//! - [`options`]: the model and effort values each ACP agent offers, as
//!   SCV last saw them.
//! - [`background`]: jobs that run while the conversation goes on.
//! - [`review`]: background jobs whose work an independent reviewer checks.
//! - [`conversation`]: handles that continue an agent's conversation.
//! - [`records`]: the on-disk record of every delegated process, for
//!   listing, stopping, and cleanup.
//! - [`stores`]: the credential files each agent CLI reads, in its own
//!   format, inside SCV's private agent homes.
//! - [`worktree`]: git worktrees SCV creates for a call's agent to work in.
//! - [`live`]: a long-lived child process read line by line.
//! - [`takeover`] and [`pty`]: the owner's takeover of a conversation in
//!   its agent's own terminal.
//! - [`output`] and [`progress`]: reading a run's output into one bounded
//!   result, and its progress into short lines.

pub(crate) mod acp;
pub mod activity;
pub mod adapters;
pub(crate) mod agent;
pub mod background;
pub mod choice;
pub mod claude_auth;
pub mod conversation;
pub(crate) mod live;
pub(crate) mod native;
pub mod options;
pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod pty;
pub mod records;
pub(crate) mod request;
pub(crate) mod review;
pub(crate) mod scv;
pub mod stores;
pub(crate) mod takeover;
pub mod worktree;
