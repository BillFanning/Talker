//! Metadata generation stage (spec §106, pipeline step in §102).
//!
//! Turns extractor output ([`MessageBytes`]) into an immutable, numbered
//! [`Message`]. Owns the per-Channel message counter and derives Payload
//! Metadata: Total Byte Count (§25), Arrival Timestamp (§26), and Reception
//! Duration (§27).

use crate::core::{ChannelId, Message, MessageBytes, MessageMetadata, MessageTimestamp};

/// Per-Channel Message Numbering (§24). Begins at 1 on Channel Start, increases
/// by one per completed Message, and resets to 1 on the next Start (§10.2).
/// Retention eviction never renumbers (§89) — this counter is monotonic for the
/// life of a Running Channel.
#[derive(Debug)]
pub struct MessageNumbering {
    next: u64,
}

impl MessageNumbering {
    /// A fresh counter, ready to assign Message Number 1 (§24).
    pub fn new() -> Self {
        Self { next: 1 }
    }

    /// Reset numbering to 1 — called when the Channel is (re)started (§10.2).
    pub fn reset(&mut self) {
        self.next = 1;
    }

    /// Number for the Message that *will* be assigned next, without consuming it.
    pub fn peek(&self) -> u64 {
        self.next
    }

    /// Assign the next Message Number and build the immutable [`Message`] (§106).
    ///
    /// Arrival Timestamp comes from the chunk that supplied the first byte (§26);
    /// Reception Duration spans the first and last contributing chunks (§27) and
    /// is zero for a Message contained in a single chunk (§105).
    pub fn build(&mut self, channel_id: ChannelId, bytes: MessageBytes) -> Message {
        let number = self.next;
        self.next += 1;

        let metadata = MessageMetadata {
            total_byte_count: bytes.bytes.len(),
            arrival_timestamp: MessageTimestamp::from(bytes.first_chunk),
            reception_duration: Some(bytes.reception_duration()),
        };

        Message {
            channel_id,
            number,
            bytes: bytes.bytes,
            metadata,
        }
    }
}

impl Default for MessageNumbering {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::core::ChunkTime;

    fn message_bytes(payload: &[u8]) -> MessageBytes {
        let t = ChunkTime::now();
        MessageBytes {
            bytes: Arc::from(payload),
            first_chunk: t,
            last_chunk: t,
        }
    }

    #[test]
    fn numbering_starts_at_one_and_increments() {
        let mut n = MessageNumbering::new();
        let cid = ChannelId::new();
        assert_eq!(n.peek(), 1);
        assert_eq!(n.build(cid, message_bytes(b"a")).number, 1);
        assert_eq!(n.build(cid, message_bytes(b"bb")).number, 2);
        assert_eq!(n.build(cid, message_bytes(b"ccc")).number, 3);
    }

    #[test]
    fn reset_returns_numbering_to_one() {
        let mut n = MessageNumbering::new();
        let cid = ChannelId::new();
        n.build(cid, message_bytes(b"a"));
        n.build(cid, message_bytes(b"b"));
        n.reset();
        assert_eq!(n.build(cid, message_bytes(b"c")).number, 1);
    }

    #[test]
    fn build_records_total_byte_count() {
        let mut n = MessageNumbering::new();
        let msg = n.build(ChannelId::new(), message_bytes(b"$GPGLL"));
        assert_eq!(msg.metadata.total_byte_count, 6);
    }
}
