//! Fixed-length extraction with optional synchronization marker (spec §22).
//!
//! Two synchronization styles (§22.1, §22.2):
//! - **Immediate** (no marker): extraction begins with the first byte received
//!   and collects `length`-byte Messages back to back.
//! - **Marker**: bytes are ignored until the marker byte sequence is observed;
//!   the marker is consumed (not part of the Message), then `length` bytes are
//!   collected as one Message, then the search repeats for the next marker
//!   (§22.2, Appendix B.4). The marker may straddle buffer boundaries.
//!
//! v1 performs no heuristic resynchronization beyond marker detection (§22.2).

use std::collections::VecDeque;
use std::sync::Arc;

use crate::core::{ChunkTime, MessageBytes};

use super::MessageExtractor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Looking for the sync marker; incoming bytes are discarded until found.
    Searching,
    /// Accumulating the `length`-byte payload of the current Message.
    Collecting,
}

/// Extracts fixed-length Messages, optionally framed by a sync marker (§22).
#[derive(Debug)]
pub struct FixedLengthExtractor {
    length: usize,
    sync_marker: Option<Vec<u8>>,
    state: State,
    /// Rolling window of the most recent bytes while `Searching`, capped at the
    /// marker length so detection works across chunk boundaries.
    window: VecDeque<u8>,
    buf: Vec<u8>,
    pending_first_chunk: Option<ChunkTime>,
}

impl FixedLengthExtractor {
    /// `length` must be greater than zero; config validation rejects zero, and a
    /// defensively-zero length simply never completes a Message. An empty
    /// `sync_marker` is treated as no marker (immediate synchronization).
    pub fn new(length: usize, sync_marker: Option<Vec<u8>>) -> Self {
        let has_marker = sync_marker.as_ref().is_some_and(|m| !m.is_empty());
        Self {
            length,
            sync_marker: sync_marker.filter(|m| !m.is_empty()),
            state: if has_marker {
                State::Searching
            } else {
                State::Collecting
            },
            window: VecDeque::new(),
            buf: Vec::new(),
            pending_first_chunk: None,
        }
    }
}

impl MessageExtractor for FixedLengthExtractor {
    fn push_chunk(&mut self, bytes: &[u8], at: ChunkTime) -> Vec<MessageBytes> {
        let mut out = Vec::new();
        if self.length == 0 {
            return out; // guard against an invalid length (see `new`)
        }
        for &b in bytes {
            match self.state {
                State::Searching => {
                    // `Searching` is only entered when a non-empty marker exists.
                    let marker = self
                        .sync_marker
                        .as_ref()
                        .expect("marker present while Searching");
                    self.window.push_back(b);
                    while self.window.len() > marker.len() {
                        self.window.pop_front();
                    }
                    if self.window.len() == marker.len()
                        && self.window.iter().copied().eq(marker.iter().copied())
                    {
                        // Marker observed and consumed; begin collecting payload.
                        self.window.clear();
                        self.buf.clear();
                        self.pending_first_chunk = None;
                        self.state = State::Collecting;
                    }
                }
                State::Collecting => {
                    if self.buf.is_empty() {
                        self.pending_first_chunk = Some(at);
                    }
                    self.buf.push(b);
                    if self.buf.len() == self.length {
                        let first = self
                            .pending_first_chunk
                            .take()
                            .expect("pending_first_chunk is set once collecting begins");
                        let bytes: Arc<[u8]> = Arc::from(self.buf.as_slice());
                        out.push(MessageBytes {
                            bytes,
                            first_chunk: first,
                            last_chunk: at,
                        });
                        self.buf.clear();
                        // With a marker, re-arm the search for the next frame;
                        // without one, keep collecting contiguous frames.
                        if self.sync_marker.is_some() {
                            self.state = State::Searching;
                        }
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<MessageBytes> {
        // A partial frame is an incomplete Message → discarded (§112).
        self.buf.clear();
        self.window.clear();
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
    fn immediate_sync_collects_back_to_back_frames() {
        let mut ex = FixedLengthExtractor::new(4, None);
        let out = ex.push_chunk(b"ABCDEFGH", ChunkTime::now());
        assert_eq!(payloads(&out), vec![b"ABCD".to_vec(), b"EFGH".to_vec()]);
    }

    #[test]
    fn frame_spanning_buffer_boundary_completes_in_later_chunk() {
        let mut ex = FixedLengthExtractor::new(4, None);
        let t1 = ChunkTime::now();
        let t2 = ChunkTime::now();
        assert!(ex.push_chunk(b"AB", t1).is_empty());
        let out = ex.push_chunk(b"CDEF", t2);
        assert_eq!(payloads(&out), vec![b"ABCD".to_vec()]);
        assert_eq!(out[0].first_chunk.monotonic, t1.monotonic);
        assert_eq!(out[0].last_chunk.monotonic, t2.monotonic);
        // "EF" remains buffered (incomplete) and is dropped on finish.
        assert!(ex.finish().is_empty());
    }

    #[test]
    fn sync_marker_frames_each_message_and_excludes_the_marker() {
        let mut ex = FixedLengthExtractor::new(3, Some(vec![0xAA, 0x55]));
        let input = [
            0x00, 0xAA, 0x55, 0x01, 0x02, 0x03, // junk, marker, frame 1
            0xAA, 0x55, 0x04, 0x05, 0x06, // marker, frame 2
        ];
        let out = ex.push_chunk(&input, ChunkTime::now());
        assert_eq!(
            payloads(&out),
            vec![vec![0x01, 0x02, 0x03], vec![0x04, 0x05, 0x06]]
        );
    }

    #[test]
    fn sync_marker_detected_across_buffer_boundary() {
        let mut ex = FixedLengthExtractor::new(3, Some(vec![0xAA, 0x55]));
        assert!(ex.push_chunk(&[0x00, 0xAA], ChunkTime::now()).is_empty());
        let out = ex.push_chunk(&[0x55, 0x01, 0x02, 0x03], ChunkTime::now());
        assert_eq!(payloads(&out), vec![vec![0x01, 0x02, 0x03]]);
    }

    #[test]
    fn bytes_before_first_marker_are_discarded() {
        let mut ex = FixedLengthExtractor::new(2, Some(vec![0x7E]));
        let out = ex.push_chunk(&[0x11, 0x22, 0x33, 0x7E, 0xAB, 0xCD], ChunkTime::now());
        assert_eq!(payloads(&out), vec![vec![0xAB, 0xCD]]);
    }
}
