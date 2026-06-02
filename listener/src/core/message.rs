//! The immutable Message model and its timing types (spec §131–§133, §138).
//!
//! A `Message` is immutable once emitted by the extractor (ADR-002): downstream
//! consumers (display, recording, decoding) share it without copying. Per-byte
//! arrival time is not available from the OS, so all timing is chunk-granular
//! (`ChunkTime`, §138) and derived in the extractor/metadata stages (§105, §106).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::ids::ChannelId;

/// A single timing capture taken when a transport chunk is read (§138).
///
/// `monotonic` orders messages, measures durations, and breaks timestamp ties
/// (§125); `wall_clock` renders Local/UTC display times (§26). Precision is not
/// accuracy — userland arrival times carry OS scheduling jitter (§26, §133).
#[derive(Clone, Copy, Debug)]
pub struct ChunkTime {
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
}

impl ChunkTime {
    /// Capture the current monotonic and wall-clock time together.
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall_clock: SystemTime::now(),
        }
    }
}

/// A Message's arrival timestamp, derived from the `ChunkTime` of the chunk that
/// supplied the Message's first byte (§133). Formatting to a display string —
/// source (Local / UTC / Relative) and resolution (s / ms / µs) — happens in the
/// display layer, never in stored state.
#[derive(Clone, Copy, Debug)]
pub struct MessageTimestamp {
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
}

impl From<ChunkTime> for MessageTimestamp {
    fn from(chunk: ChunkTime) -> Self {
        Self {
            monotonic: chunk.monotonic,
            wall_clock: chunk.wall_clock,
        }
    }
}

/// Completed Message bytes as emitted by the extractor (§139), before the
/// metadata stage assigns a Message Number (§24).
///
/// The extractor associates each completed Message with the `ChunkTime` of the
/// chunk that supplied its first byte and the chunk that supplied its last byte
/// (§105); the metadata stage uses these to compute the Arrival Timestamp (§26)
/// and Reception Duration (§27).
#[derive(Clone, Debug)]
pub struct MessageBytes {
    pub bytes: Arc<[u8]>,
    pub first_chunk: ChunkTime,
    pub last_chunk: ChunkTime,
}

impl MessageBytes {
    /// Reception Duration (§27): elapsed monotonic time between the first and
    /// last contributing chunk. A Message wholly contained in one chunk has a
    /// duration of zero (§105).
    pub fn reception_duration(&self) -> Duration {
        self.last_chunk
            .monotonic
            .saturating_duration_since(self.first_chunk.monotonic)
    }
}

/// Protocol-independent Payload Metadata for a completed Message (§4.7, §132).
#[derive(Clone, Copy, Debug)]
pub struct MessageMetadata {
    /// Total Byte Count of the completed Message (§25).
    pub total_byte_count: usize,
    /// Arrival Timestamp — receipt of the Message's first byte (§26).
    pub arrival_timestamp: MessageTimestamp,
    /// Optional Reception Duration (§27); `None` in Stream Mode or when not
    /// computed.
    pub reception_duration: Option<Duration>,
}

/// An immutable, completed Message (§4.3, §131).
///
/// Message bytes are immutable; the Message Number is Channel-local, resets on
/// Start, and exists only in Message Mode (§24).
#[derive(Clone, Debug)]
pub struct Message {
    pub channel_id: ChannelId,
    pub number: u64,
    pub bytes: Arc<[u8]>,
    pub metadata: MessageMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_message_has_zero_reception_duration() {
        let t = ChunkTime::now();
        let mb = MessageBytes {
            bytes: Arc::from(b"$GPGLL".as_slice()),
            first_chunk: t,
            last_chunk: t,
        };
        assert_eq!(mb.reception_duration(), Duration::ZERO);
    }

    #[test]
    fn arrival_timestamp_is_taken_from_the_first_chunk() {
        let first = ChunkTime::now();
        let last = ChunkTime::now();
        let mb = MessageBytes {
            bytes: Arc::from(b"data".as_slice()),
            first_chunk: first,
            last_chunk: last,
        };
        let ts = MessageTimestamp::from(mb.first_chunk);
        assert_eq!(ts.monotonic, first.monotonic);
    }
}
