//! Delegated agents run end to end: background jobs, a nested SCV, progress,
//! and cleanup after their owner dies.

mod background;
mod nested_scv;
mod progress;
#[cfg(target_os = "linux")]
mod reconcile;
