//! Bounded history and eviction policies (spec §86–§90, §143).
//!
//! This is `listener-retention` (§128). Retention is runtime-only and never
//! affects Recording (§86). All stores are bounded (§124): when a limit is
//! exceeded the oldest items are evicted first (§89). In the stream-only design
//! (ADR-010) the byte-bounded stream scrollback lives in the pipeline; this
//! module provides the count-limited [`CountBounded`] history used for events,
//! warnings, and errors.

use std::collections::VecDeque;

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
