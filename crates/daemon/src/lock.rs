//! Poison-recovering access to `std::sync::Mutex`.
//!
//! A poisoned lock means some thread panicked while holding it; the protected
//! data is mid-update but structurally valid (a grid, an `Option`, an `Arc`).
//! For the daemon's hot locks, recovering and continuing is strictly better
//! than propagating the panic to every other locker, which would cascade one
//! pane's failure into a session-wide render wedge. See the
//! terminal-trust-hardening spec, Phase 1.

use std::sync::{Mutex, MutexGuard, PoisonError};

pub trait LockExt<T> {
    /// Lock, recovering the guard from a poisoned lock instead of panicking.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        // `PoisonError` carries the guard; take it and move on.
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    #[test]
    fn lock_recover_returns_inner_after_poison() {
        let m = Arc::new(Mutex::new(7));
        let m2 = Arc::clone(&m);
        // Poison the mutex: panic while holding the guard.
        let _ = thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        // The std lock now reports poisoned…
        assert!(m.lock().is_err(), "precondition: lock is poisoned");
        // …but `lock_recover` still yields the value.
        assert_eq!(*m.lock_recover(), 7);
    }

    #[test]
    fn lock_recover_works_when_unpoisoned() {
        let m = Mutex::new(3);
        *m.lock_recover() = 4;
        assert_eq!(*m.lock_recover(), 4);
    }
}
