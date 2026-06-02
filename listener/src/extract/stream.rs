//! Stream Mode: no Message boundaries are recognized (spec §18).

use crate::core::{ChunkTime, MessageBytes};

use super::MessageExtractor;

/// Stream Mode extractor. Recognizes no boundaries, so it produces no Messages:
/// Message Numbering and Message timestamps are not applicable in Stream Mode
/// (§18, §24). Stream bytes reach display and Raw Recording through the
/// pre-extraction chunk path (§53), not as Messages.
#[derive(Debug, Default)]
pub struct StreamExtractor;

impl StreamExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl MessageExtractor for StreamExtractor {
    fn push_chunk(&mut self, _bytes: &[u8], _at: ChunkTime) -> Vec<MessageBytes> {
        Vec::new()
    }

    fn finish(&mut self) -> Vec<MessageBytes> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_mode_never_emits_messages() {
        let mut ex = StreamExtractor::new();
        assert!(ex
            .push_chunk(b"anything\r\nat all", ChunkTime::now())
            .is_empty());
        assert!(ex.finish().is_empty());
    }
}
