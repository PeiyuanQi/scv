//! Helpers the crate's unit tests share.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use scv_client::Layout;
use scv_tools::delegation::DelegationRegistry;

use crate::config::{ConfigOverrides, Instance};

/// Sets its flag when dropped, to show a task's future was dropped.
pub(crate) struct DropSignal(pub(crate) Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A delegation registry in a private temporary instance home.
pub(crate) fn test_registry() -> Arc<DelegationRegistry> {
    let home = tempfile::tempdir().unwrap().keep();
    Arc::new(DelegationRegistry::new(&Layout::new(&home)))
}

/// The instance at `home`, with no command-line overrides.
pub(crate) fn test_instance(home: impl Into<std::path::PathBuf>) -> Instance {
    Instance {
        layout: Layout::new(home),
        overrides: ConfigOverrides::default(),
    }
}

/// Shell lines for a fake Codex: the rollout `turn_context` real Codex
/// writes under `$CODEX_HOME/sessions/` for thread `thread`, recording the
/// mode its arguments asked for, which SCV's mode check reads.
pub(crate) fn codex_rollout(thread: &str) -> String {
    format!(
        r#"mode='"approval_policy":"on-request","sandbox_policy":{{"type":"workspace-write"}}'
for arg in "$@"; do
  [ "$arg" = --dangerously-bypass-approvals-and-sandbox ] && mode='"approval_policy":"never","sandbox_policy":{{"type":"danger-full-access"}}'
done
mkdir -p "$CODEX_HOME/sessions/2026/10/30"
echo "{{\"type\":\"turn_context\",\"payload\":{{$mode}}}}" >> "$CODEX_HOME/sessions/2026/10/30/rollout-2026-10-30T00-00-00-{thread}.jsonl"
"#
    )
}
