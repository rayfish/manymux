//! Taking a lock without inheriting a panic.
//!
//! Every mutex in here guards a short, synchronous critical section, which is
//! what a `std` mutex is for: a `tokio` one buys nothing where nothing awaits,
//! and [`Drop`] cannot await at all, which is where an attachment gives its
//! size back ([`crate::node::session`]). The clippy lint in CI is what keeps
//! that true rather than merely believed.
//!
//! What `std` adds on top of the lock is poisoning, and that part is not worth
//! having here. A panic under a lock is a bug in one operation; poisoning turns
//! it into every later operation panicking too, so a node whose session
//! bookkeeping panicked once answers nothing about that session until somebody
//! restarts it, taking every other session with it. The data is no less
//! readable for having been left mid-update, and a session you can still reach
//! beats a correct refusal to look at it.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock it, panic or no panic.
pub fn held<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
