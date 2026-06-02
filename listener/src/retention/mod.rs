//! Bounded history and eviction policies (spec §86–§90, §143).
//!
//! This is `listener-retention` (§128). Retention is runtime-only and never
//! affects Recording (§86). All stores are bounded (§124): when a limit is
//! exceeded the oldest items are evicted first (§89), and Message Numbers are
//! never rewritten by eviction (§89). A hard backstop bounds memory even when a
//! configuration leaves every limit unset (§80).
//!
//! Two stores cover the §88 limit kinds:
//! - [`MessageRetention`] — bounded by message count *and* total byte count.
//! - [`CountBounded`] — count-limited history for events, warnings, and errors.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::core::Message;

/// Hard implementation cap on retained item count (§80, §124). Applied in
/// addition to any configured limit so retention is always bounded — even if a
/// hand-edited profile reaches the runtime with every limit unset.
pub const DEFAULT_BACKSTOP: usize = 1 << 20;

/// A bounded, runtime-only history of items (§143). Eviction is oldest-first
/// (§89); `clear` discards all retained items (§90).
pub trait RetentionStore<T> {
    fn push(&mut self, item: T);
    fn clear(&mut self);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Bounded Message history (§86–§89). Stays within an optional message-count
/// limit and an optional total-byte limit (§88), evicting oldest-first; the most
/// recent Message is always retained, so a single Message larger than the byte
/// limit is kept rather than dropping everything.
pub struct MessageRetention {
    message_limit: Option<usize>,
    byte_limit: Option<usize>,
    backstop: usize,
    items: VecDeque<Arc<Message>>,
    total_bytes: usize,
}

impl MessageRetention {
    /// A store with the given limits (`None` = that limit not enforced) and the
    /// default backstop. Validation (§80) rejects an all-`None` `RetentionConfig`
    /// upstream; the backstop is the defense in depth if one slips through.
    pub fn new(message_limit: Option<usize>, byte_limit: Option<usize>) -> Self {
        Self {
            message_limit,
            byte_limit,
            backstop: DEFAULT_BACKSTOP,
            items: VecDeque::new(),
            total_bytes: 0,
        }
    }

    /// Override the hard backstop count (mainly for tests / tuning).
    pub fn with_backstop(mut self, backstop: usize) -> Self {
        self.backstop = backstop.max(1);
        self
    }

    /// Total bytes currently retained across all Messages.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Message>> {
        self.items.iter()
    }

    /// Whether the store currently exceeds any active bound.
    fn over_limits(&self) -> bool {
        if self.items.len() > self.backstop {
            return true;
        }
        if self.message_limit.is_some_and(|m| self.items.len() > m) {
            return true;
        }
        self.byte_limit.is_some_and(|b| self.total_bytes > b)
    }
}

impl RetentionStore<Arc<Message>> for MessageRetention {
    fn push(&mut self, item: Arc<Message>) {
        self.total_bytes += item.bytes.len();
        self.items.push_back(item);
        // Evict oldest-first until within bounds, but never drop the newest
        // Message (always retain at least one).
        while self.items.len() > 1 && self.over_limits() {
            if let Some(evicted) = self.items.pop_front() {
                self.total_bytes -= evicted.bytes.len();
            }
        }
    }

    fn clear(&mut self) {
        self.items.clear();
        self.total_bytes = 0;
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

/// A count-limited history for Events, Warnings, or Errors (§86, §88). Oldest
/// entries are evicted first (§89).
pub struct CountBounded<T> {
    limit: usize,
    items: VecDeque<T>,
}

impl<T> CountBounded<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            items: VecDeque::new(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }
}

impl<T> RetentionStore<T> for CountBounded<T> {
    fn push(&mut self, item: T) {
        self.items.push_back(item);
        while self.items.len() > self.limit {
            self.items.pop_front();
        }
    }

    fn clear(&mut self) {
        self.items.clear();
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, MessageMetadata, MessageTimestamp};

    fn message(number: u64, len: usize) -> Arc<Message> {
        Arc::new(Message {
            channel_id: ChannelId::new(),
            number,
            bytes: Arc::from(vec![0u8; len].as_slice()),
            metadata: MessageMetadata {
                total_byte_count: len,
                arrival_timestamp: MessageTimestamp::from(ChunkTime::now()),
                reception_duration: None,
            },
        })
    }

    fn numbers(store: &MessageRetention) -> Vec<u64> {
        store.iter().map(|m| m.number).collect()
    }

    #[test]
    fn message_count_limit_evicts_oldest_without_renumbering() {
        let mut store = MessageRetention::new(Some(3), None);
        for n in 1..=5 {
            store.push(message(n, 1));
        }
        // Oldest two evicted; survivors keep their original numbers (§89).
        assert_eq!(numbers(&store), vec![3, 4, 5]);
    }

    #[test]
    fn byte_limit_evicts_until_within_budget() {
        let mut store = MessageRetention::new(None, Some(10));
        for n in 1..=4 {
            store.push(message(n, 4)); // 4 bytes each
        }
        // 16 bytes pushed; budget 10 → keep newest fitting: 8 bytes (msgs 3,4).
        assert_eq!(numbers(&store), vec![3, 4]);
        assert_eq!(store.total_bytes(), 8);
    }

    #[test]
    fn the_tighter_of_the_two_limits_binds() {
        // Count allows 10, but bytes cap at 6 (2 msgs of 3 bytes).
        let mut store = MessageRetention::new(Some(10), Some(6));
        for n in 1..=5 {
            store.push(message(n, 3));
        }
        assert_eq!(numbers(&store), vec![4, 5]);
    }

    #[test]
    fn newest_message_is_kept_even_if_it_alone_exceeds_the_byte_limit() {
        let mut store = MessageRetention::new(None, Some(4));
        store.push(message(1, 100));
        assert_eq!(numbers(&store), vec![1]);
        assert_eq!(store.total_bytes(), 100);
    }

    #[test]
    fn backstop_bounds_an_unlimited_store() {
        // No configured limits — only the backstop keeps it bounded (§80).
        let mut store = MessageRetention::new(None, None).with_backstop(2);
        for n in 1..=5 {
            store.push(message(n, 1));
        }
        assert_eq!(numbers(&store), vec![4, 5]);
    }

    #[test]
    fn clear_discards_everything() {
        let mut store = MessageRetention::new(Some(10), None);
        store.push(message(1, 5));
        store.push(message(2, 5));
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn count_bounded_log_evicts_oldest() {
        let mut log: CountBounded<&str> = CountBounded::new(2);
        log.push("a");
        log.push("b");
        log.push("c");
        assert_eq!(log.iter().copied().collect::<Vec<_>>(), vec!["b", "c"]);
        log.clear();
        assert!(log.is_empty());
    }
}
