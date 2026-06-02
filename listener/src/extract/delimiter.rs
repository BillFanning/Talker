//! Delimiter-based extraction (spec §21).
//!
//! A Message is complete when the configured delimiter sequence is observed.
//! The delimiter may be one or more arbitrary bytes and may straddle receive-
//! buffer boundaries (§21) — the partial-message buffer persists across chunks,
//! so detection works the same whether the delimiter arrives whole or split.

use std::sync::Arc;

use crate::core::{ChunkTime, MessageBytes};

use super::MessageExtractor;

/// Extracts Messages terminated by a fixed delimiter byte sequence (§21).
///
/// Each delimiter occurrence ends a Message, including a delimiter with no
/// preceding payload bytes (consecutive delimiters yield empty-payload
/// Messages, mirroring standard split semantics; see listener ADR-005). When
/// `include_delimiter` is false the delimiter bytes are stripped from the
/// emitted payload but still mark completion.
#[derive(Debug)]
pub struct DelimiterExtractor {
    delimiter: Vec<u8>,
    include_delimiter: bool,
    buf: Vec<u8>,
    /// `ChunkTime` of the chunk that supplied `buf[0]` — the first byte of the
    /// Message currently being assembled (§105).
    pending_first_chunk: Option<ChunkTime>,
}

impl DelimiterExtractor {
    /// `delimiter` must be non-empty; config validation rejects an empty
    /// delimiter, and a defensively-empty one simply never completes a Message.
    pub fn new(delimiter: Vec<u8>, include_delimiter: bool) -> Self {
        Self {
            delimiter,
            include_delimiter,
            buf: Vec::new(),
            pending_first_chunk: None,
        }
    }

    /// Complete the buffered Message. `last` is the `ChunkTime` of the chunk
    /// that supplied the final delimiter byte — the byte that completes the
    /// Message (§27) — regardless of whether the delimiter is included.
    fn take_message(&mut self, last: ChunkTime) -> MessageBytes {
        let first = self
            .pending_first_chunk
            .take()
            .expect("pending_first_chunk is set whenever buf is non-empty");
        let payload_len = if self.include_delimiter {
            self.buf.len()
        } else {
            self.buf.len() - self.delimiter.len()
        };
        let bytes: Arc<[u8]> = Arc::from(&self.buf[..payload_len]);
        self.buf.clear();
        MessageBytes {
            bytes,
            first_chunk: first,
            last_chunk: last,
        }
    }
}

impl MessageExtractor for DelimiterExtractor {
    fn push_chunk(&mut self, bytes: &[u8], at: ChunkTime) -> Vec<MessageBytes> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.buf.is_empty() {
                self.pending_first_chunk = Some(at);
            }
            self.buf.push(b);
            if !self.delimiter.is_empty() && self.buf.ends_with(&self.delimiter) {
                out.push(self.take_message(at));
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<MessageBytes> {
        // Un-terminated trailing data is an incomplete Message → discarded (§112).
        self.buf.clear();
        self.pending_first_chunk = None;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payloads(msgs: &[MessageBytes]) -> Vec<Vec<u8>> {
        msgs.iter().map(|m| m.bytes.to_vec()).collect()
    }

    #[test]
    fn splits_on_single_byte_delimiter_excluded() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], false);
        let out = ex.push_chunk(b"A\nB\n", ChunkTime::now());
        assert_eq!(payloads(&out), vec![b"A".to_vec(), b"B".to_vec()]);
    }

    #[test]
    fn delimiter_can_be_included_in_payload() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], true);
        let out = ex.push_chunk(b"A\nB\n", ChunkTime::now());
        assert_eq!(payloads(&out), vec![b"A\n".to_vec(), b"B\n".to_vec()]);
    }

    #[test]
    fn single_byte_delimiter_across_buffer_boundary() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], false);
        let t1 = ChunkTime::now();
        let t2 = ChunkTime::now();
        assert!(ex.push_chunk(b"AB", t1).is_empty());
        let out = ex.push_chunk(b"C\nD\n", t2);
        assert_eq!(payloads(&out), vec![b"ABC".to_vec(), b"D".to_vec()]);
        // "ABC" began in chunk 1 and completed in chunk 2.
        assert_eq!(out[0].first_chunk.monotonic, t1.monotonic);
        assert_eq!(out[0].last_chunk.monotonic, t2.monotonic);
        // "D" began and completed in chunk 2.
        assert_eq!(out[1].first_chunk.monotonic, t2.monotonic);
    }

    #[test]
    fn multi_byte_delimiter_across_buffer_boundary() {
        let mut ex = DelimiterExtractor::new(b"\r\n".to_vec(), false);
        assert!(ex.push_chunk(b"X\r", ChunkTime::now()).is_empty());
        let out = ex.push_chunk(b"\nY\r\n", ChunkTime::now());
        assert_eq!(payloads(&out), vec![b"X".to_vec(), b"Y".to_vec()]);
    }

    #[test]
    fn consecutive_delimiters_yield_empty_messages() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], false);
        let out = ex.push_chunk(b"\n\n", ChunkTime::now());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|m| m.bytes.is_empty()));
    }

    #[test]
    fn consecutive_delimiters_included_keep_the_delimiter() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], true);
        let out = ex.push_chunk(b"A\n\nB\n", ChunkTime::now());
        assert_eq!(
            payloads(&out),
            vec![b"A\n".to_vec(), b"\n".to_vec(), b"B\n".to_vec()]
        );
    }

    #[test]
    fn missing_delimiter_emits_nothing_and_discards_on_finish() {
        let mut ex = DelimiterExtractor::new(vec![b'\n'], false);
        assert!(ex.push_chunk(b"ABC", ChunkTime::now()).is_empty());
        assert!(ex.finish().is_empty());
    }
}
