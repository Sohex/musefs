//! Poison-recovery policy for the daemon's in-memory mutexes (#96).
//!
//! musefs is read-only and the SQLite store is the source of truth, so on a
//! poisoned lock we reset the guarded state to a known-good value rather than
//! serve possibly-inconsistent state:
//!   * caches  -> `lock_or_clear`  (clear; next access cold-resolves from the DB)
//!   * VFS state -> `lock_or_flag` (schedule a full rebuild via `poll_refresh`)
//!   * scalars -> `lock_recover`   (replace-only writes can't be half-written)

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// State that can be reset to empty for `lock_or_clear`.
pub(crate) trait Clearable {
    fn reset(&mut self);
}

impl<T> Clearable for Option<T> {
    fn reset(&mut self) {
        *self = None;
    }
}

/// Category 3 — transient scalar. Recover the inner value, logging the poison.
pub(crate) fn lock_recover<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| {
        log::error!("recovered poisoned scalar lock ({what}); continuing on inner value");
        e.into_inner()
    })
}

/// Category 1 — cache. On poison, clear the cache so the next access
/// cold-resolves; a cleared cache cannot be inconsistent.
pub(crate) fn lock_or_clear<'a, T: Clearable>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => {
            log::error!("cleared poisoned cache lock ({what})");
            let mut g = e.into_inner();
            g.reset();
            g
        }
    }
}

/// Category 2 — rebuildable VFS state. On poison, flag a full rebuild (run by the
/// next `poll_refresh`) and recover the inner value for best-effort completion.
pub(crate) fn lock_or_flag<'a, T>(
    m: &'a Mutex<T>,
    needs_rebuild: &AtomicBool,
    what: &str,
) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| {
        log::error!("poisoned VFS-state lock ({what}); scheduling full rebuild");
        needs_rebuild.store(true, Ordering::Release);
        e.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn poison<T: Send + 'static>(m: &Arc<Mutex<T>>) {
        let m2 = Arc::clone(m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
    }

    #[test]
    fn recover_returns_inner_after_poison() {
        let m = Arc::new(Mutex::new(7u32));
        poison(&m);
        assert_eq!(*lock_recover(&m, "scalar"), 7);
    }

    #[test]
    fn clear_empties_cache_after_poison() {
        let m = Arc::new(Mutex::new(Some(42u32)));
        poison(&m);
        assert!(lock_or_clear(&m, "cache").is_none());
    }

    #[test]
    fn flag_set_after_poison() {
        let m = Arc::new(Mutex::new(0u32));
        let flag = AtomicBool::new(false);
        poison(&m);
        let _g = lock_or_flag(&m, &flag, "vfs");
        assert!(flag.load(Ordering::Acquire));
    }
}
