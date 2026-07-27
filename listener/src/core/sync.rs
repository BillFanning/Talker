//! One poison-recovery policy for the small shared state cells (§161, ADR-006).
//!
//! Every `Mutex` in Listener guards a small `Copy` snapshot — serial control
//! lines, serial stall totals — that a panicking holder cannot leave logically
//! torn: the guarded writes are single assignments, not multi-step invariants.
//! Recovering the inner value is therefore always safe and always more useful
//! than the alternatives, which silently degrade an unrelated signal:
//!
//! - Skipping the update (`if let Ok(guard)`) drops a live control-line change
//!   with no diagnostic, so the panel freezes on stale values.
//! - Propagating `None` is worse still where `None` already means something
//!   else — a poisoned control-line cell would read as "not a serial channel"
//!   and hide the whole panel.
//!
//! A poisoned cell means some other thread panicked; that fault surfaces
//! through the transport outcome and the diagnostics log, which is where an
//! operator should learn about it — not by watching a readout quietly stop.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering the guarded value if a previous holder panicked.
pub fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_cell_still_yields_its_last_value() {
        let cell = Arc::new(Mutex::new(7u32));
        let poisoner = Arc::clone(&cell);
        // Panic while holding the lock, poisoning it for every later caller.
        let _ = std::thread::spawn(move || {
            let mut guard = lock_recover(&poisoner);
            *guard = 9;
            panic!("poison the cell");
        })
        .join();

        assert!(cell.lock().is_err(), "the cell is genuinely poisoned");
        assert_eq!(*lock_recover(&cell), 9, "the last write is still readable");
        *lock_recover(&cell) = 11;
        assert_eq!(*lock_recover(&cell), 11, "writes keep working after poison");
    }
}
