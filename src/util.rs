//! Small utilities shared across modules.

use std::sync::{Mutex, MutexGuard};

/// Lock a [`std::sync::Mutex`] and transparently recover from poisoning.
///
/// Every place that holds a `std::sync::Mutex` in this crate is happy to read
/// the inner value even after a panic on another thread, so `unwrap_or_else
/// (|e| e.into_inner())` is the uniform policy; this helper removes the
/// boilerplate.
pub fn lock_sync<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
