//! Locking a standard mutex the same way everywhere.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering its value if a thread panicked while holding it.
///
/// The state behind these locks (job tables, run records, conversation maps)
/// stays consistent between statements, so a panic elsewhere must not turn
/// every later tool call into a second panic.
pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
