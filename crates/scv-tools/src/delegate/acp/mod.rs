//! Delegation over the Agent Client Protocol (ACP): JSON-RPC 2.0 over stdio.
//!
//! Each conversation runs one ACP server
//! ([`LiveChild`](crate::delegate::live::LiveChild)): the agent itself
//! (`grok agent stdio`, `dsh --profile acp`) or its official adapter
//! (`claude-agent-acp`, `codex-acp`). The first turn performs `initialize`
//! and `session/new`; every turn is a `session/prompt` on that session.
//! `session/update` notifications become progress, the agent's
//! `session/request_permission` goes through the calling session's approval
//! gate, and a cancelled or timed-out call sends `session/cancel`. SCV offers
//! no client file-system or terminal capabilities, so every other request the
//! agent makes is refused as an unknown method.

mod permission;
mod progress;
mod rpc;
mod session;
mod tool;

pub(crate) use tool::AcpAgentTool;
// The submodules share one namespace through `use super::*`.
use {
    permission::{choose_option, describe_permission},
    progress::Progress,
    rpc::{CallError, Incoming, Interrupt, Rpc, describe_rpc_error},
    session::{AcpChild, TurnEnd},
};

#[cfg(test)]
mod tests;
