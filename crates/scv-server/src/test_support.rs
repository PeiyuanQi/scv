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
