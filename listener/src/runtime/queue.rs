//! Bounded-queue backpressure policies (spec §99, §99.1).
//!
//! Every fan-out edge is bounded (§124). Most §99 edges are bounded
//! `tokio::sync::mpsc` channels: the Transport→Pipeline edge (the only one that
//! may stall the reader, §97.1, wired in [`super::pipeline`]) and the recorder
//! queues (whose `try_send` rejection faults the recording, §56.1 — see
//! [`Recording`](crate::record::Recording)). This module holds the remaining
//! synchronous policy type — [`DropOldestQueue`], the §99 "drop oldest" edge —
//! kept plain so its invariant is unit-testable without timing.
//!
//! Diagnostics retention (§88/§92–§95) is **not** a queue: it lives in the
//! [`DiagnosticLog`](crate::diagnostics::DiagnosticLog), which bounds each
//! severity independently (count-capped, oldest evicted). None of the fan-out
//! policies ever block a producer.

use std::collections::VecDeque;

/// A bounded queue that discards the **oldest** item to admit a new one when
/// full (§99: "Drop oldest" / "Evict oldest"). Used for the bounded
/// recent-match log (§165). Never blocks; never rejects the newest item.
#[derive(Debug)]
pub struct DropOldestQueue<T> {
    cap: usize,
    items: VecDeque<T>,
}

impl<T> DropOldestQueue<T> {
    /// Capacity is clamped to at least 1 so a queue always admits the newest
    /// item (and so a stray all-`None` retention config cannot disable the
    /// backstop, §80).
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            items: VecDeque::new(),
        }
    }

    /// Push the newest item, evicting and returning the oldest if at capacity.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = if self.items.len() >= self.cap {
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(item);
        evicted
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_oldest_evicts_front_and_keeps_newest() {
        let mut q = DropOldestQueue::with_capacity(2);
        assert_eq!(q.push(1), None);
        assert_eq!(q.push(2), None);
        // Full: pushing 3 evicts the oldest (1).
        assert_eq!(q.push(3), Some(1));
        assert_eq!(q.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn drop_oldest_capacity_is_at_least_one() {
        let mut q = DropOldestQueue::with_capacity(0);
        assert_eq!(q.push("a"), None);
        assert_eq!(q.push("b"), Some("a"));
    }
}
