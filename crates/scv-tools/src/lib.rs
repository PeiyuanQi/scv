//! SCV's bounded, workspace-aware built-in tools.
//!
//! [`builtin_registry`] builds a session's tools: files (`read`,
//! `read_skill`, `write`), the shell (`bash`), the web (`web_fetch`,
//! `web_search`), delegation to other agent CLIs and to a nested SCV
//! (`agent_*`, with background jobs), and `chat_attach` for chat sessions.
//! [`delegation`] records every delegated process so SCV can list, stop, and
//! clean them up.
//!
//! The source is laid out by what a tool does:
//!
//! - `builtin/`: tools that run inside SCV (`fs`, `skill`, `shell`, [`web`],
//!   [`chat_attach`]).
//! - `delegate/`: the `agent_*` tools that hand a turn to another agent: one
//!   CLI process per turn (`native`), a long-lived Agent Client Protocol
//!   server (`acp`), or a nested SCV (`scv`); the [`adapters`] table,
//!   [`background`] jobs, [`conversation`] handles, run records
//!   ([`delegation`]), and how a run's output and progress are read.
//! - `process`: spawning a child in its own process group and always
//!   finishing the whole group.
//! - `registry`, `config`, and `args`: assembling a session's tools, their
//!   settings, and the argument helpers they share.

mod args;
mod builtin;
mod config;
mod delegate;
mod process;
mod registry;
mod sync;

pub use builtin::{chat_attach, web};
pub use config::{AcpAgentLaunch, AgentAdapterConfig, DelegationContext, SkillMap, ToolsConfig};
pub use delegate::{
    adapters, background, choice as agent_choice, conversation, records as delegation,
};
pub use process::apply_agent_environment;
pub use registry::builtin_registry;
