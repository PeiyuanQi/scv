//! Helpers the crate's unit tests share.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use scv_tools::delegation::DelegationRegistry;

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
    Arc::new(DelegationRegistry::new(&home))
}
