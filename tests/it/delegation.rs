//! Delegated agents run end to end: background jobs, reviewed jobs, a nested
//! SCV, progress, and cleanup after their owner dies.

mod background;
mod depth;
mod nested_scv;
mod progress;
#[cfg(target_os = "linux")]
mod reconcile;
mod review;
