//! Bounded-queue backpressure policies (spec §99, §99.1).
//!
//! Every fan-out edge is bounded (§124). The §99 matrix assigns each edge an
//! overflow behavior; this module provides the concrete, deterministic policy
//! types those edges use. They are intentionally plain and synchronous so the
//! §152 backpressure invariants can be unit-tested without timing.
//!
//! | §99 edge | policy type |
//! |---|---|
//! | Fan-out → Display, Fan-out → Retention | [`DropOldestQueue`] |
//! | Chunk tap → Raw Recording, Fan-out → Display Recording | [`FaultOnFullQueue`] |
//!
//! Diagnostics retention (§88/§92–§95) is **not** one of these queue types: it lives in
//! the [`DiagnosticLog`](crate::diagnostics::DiagnosticLog), which bounds each severity
//! independently (count-capped, oldest evicted).
//!
//! Only the Transport→Pipeline edge may stall the reader (§97.1); it is a
//! bounded `tokio::sync::mpsc` channel wired in [`super::pipeline`], not one of
//! these types — none of these ever block a producer.

use std::collections::VecDeque;

/// A bounded queue that discards the **oldest** item to admit a new one when
/// full (§99: "Drop oldest" / "Evict oldest"). Used for display fan-out and for
/// retention eviction (§89). Never blocks; never rejects the newest item.
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

    /// Remove and return the oldest item (consumer side / FIFO drain).
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
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

/// The newest item was rejected because the queue was full. For recording edges
/// this triggers a recorder fault rather than blocking the producer (§56.1).
#[derive(Debug, PartialEq, Eq)]
pub struct QueueFull<T>(pub T);

/// A bounded queue that **rejects** the new item when full (§99: `try_send` →
/// fault). Used by the raw and display recorders, which transition to `Faulted`
/// on rejection and never stall reception (§56.1).
#[derive(Debug)]
pub struct FaultOnFullQueue<T> {
    cap: usize,
    items: VecDeque<T>,
}

impl<T> FaultOnFullQueue<T> {
    /// A capacity of 0 means every push is rejected (useful to force-fault in
    /// tests); production recorders use a real bound.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            items: VecDeque::new(),
        }
    }

    /// Try to enqueue. Returns `Err(QueueFull(item))` if at capacity, handing
    /// the rejected item back so the caller can fault deterministically.
    pub fn try_push(&mut self, item: T) -> Result<(), QueueFull<T>> {
        if self.items.len() >= self.cap {
            return Err(QueueFull(item));
        }
        self.items.push_back(item);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
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

    #[test]
    fn fault_on_full_rejects_and_returns_the_item() {
        let mut q = FaultOnFullQueue::with_capacity(1);
        assert!(q.try_push(10).is_ok());
        assert_eq!(q.try_push(20), Err(QueueFull(20)));
        // Draining frees room again.
        assert_eq!(q.pop(), Some(10));
        assert!(q.try_push(20).is_ok());
    }
}
