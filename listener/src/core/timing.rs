//! Chunk-arrival timing types (spec §138).
//!
//! In the stream-only design (ADR-010) there is no Message model; the only
//! timing the runtime needs is the per-chunk arrival time. Per-byte arrival time
//! is not available from the OS, so all timing is chunk-granular (`ChunkTime`,
//! §138): a chunk's [`ChunkTime`] is captured when the transport reads it, and a
//! recording timestamp ([`ChunkTimestamp`]) is derived from it.

use std::time::{Instant, SystemTime};

/// A single timing capture taken when a transport chunk is read (§138).
///
/// `monotonic` orders chunks, measures durations, and breaks timestamp ties
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

/// A timestamp derived from the `ChunkTime` of a received chunk (§133), carried
/// alongside rendered/recorded output. Formatting to a display string — source
/// (Local / UTC / Relative) and resolution (s / ms / µs) — happens in the display
/// layer, never in stored state.
#[derive(Clone, Copy, Debug)]
pub struct ChunkTimestamp {
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
}

impl From<ChunkTime> for ChunkTimestamp {
    fn from(chunk: ChunkTime) -> Self {
        Self {
            monotonic: chunk.monotonic,
            wall_clock: chunk.wall_clock,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_taken_from_the_chunk_time() {
        let chunk = ChunkTime::now();
        let ts = ChunkTimestamp::from(chunk);
        assert_eq!(ts.monotonic, chunk.monotonic);
        assert_eq!(ts.wall_clock, chunk.wall_clock);
    }
}
