use serde::{Deserialize, Serialize};

use super::byte_order::ByteOrder;

/// A single typed, byte-order-aware binary field that can be encoded to bytes.
///
/// `U24` stores its value in the lower 24 bits of a `u32`; values ≥ 2²⁴ are
/// silently truncated on encode.  This matches Sea-Bird instrument conventions
/// where many sensor words are 24-bit unsigned big-endian integers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BinaryField {
    U8(u8),
    U16(u16, ByteOrder),
    U24(u32, ByteOrder),
    U32(u32, ByteOrder),
    U64(u64, ByteOrder),
    I8(i8),
    I16(i16, ByteOrder),
    I32(i32, ByteOrder),
    I64(i64, ByteOrder),
    F32(f32, ByteOrder),
    F64(f64, ByteOrder),
    RawBytes(Vec<u8>),
}

impl BinaryField {
    /// Number of bytes this field encodes to.
    pub fn byte_len(&self) -> usize {
        match self {
            Self::U8(_) | Self::I8(_) => 1,
            Self::U16(_, _) | Self::I16(_, _) => 2,
            Self::U24(_, _) => 3,
            Self::U32(_, _) | Self::I32(_, _) | Self::F32(_, _) => 4,
            Self::U64(_, _) | Self::I64(_, _) | Self::F64(_, _) => 8,
            Self::RawBytes(v) => v.len(),
        }
    }

    /// Encode this field into `buf`.
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        match self {
            Self::U8(v) => buf.push(*v),
            Self::I8(v) => buf.push(*v as u8),
            Self::U16(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::U16(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::U24(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()[1..]),
            Self::U24(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()[..3]),
            Self::U32(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::U32(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::U64(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::U64(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::I16(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::I16(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::I32(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::I32(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::I64(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::I64(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::F32(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::F32(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::F64(v, ByteOrder::BigEndian) => buf.extend_from_slice(&v.to_be_bytes()),
            Self::F64(v, ByteOrder::LittleEndian) => buf.extend_from_slice(&v.to_le_bytes()),
            Self::RawBytes(v) => buf.extend_from_slice(v),
        }
    }

    /// Encode this field into a new `Vec<u8>`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.byte_len());
        self.encode_into(&mut buf);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── byte_len ─────────────────────────────────────────────────────────────

    #[test]
    fn byte_len_scalars() {
        assert_eq!(BinaryField::U8(0).byte_len(), 1);
        assert_eq!(BinaryField::I8(0).byte_len(), 1);
        assert_eq!(BinaryField::U16(0, ByteOrder::BigEndian).byte_len(), 2);
        assert_eq!(BinaryField::I16(0, ByteOrder::BigEndian).byte_len(), 2);
        assert_eq!(BinaryField::U24(0, ByteOrder::BigEndian).byte_len(), 3);
        assert_eq!(BinaryField::U32(0, ByteOrder::BigEndian).byte_len(), 4);
        assert_eq!(BinaryField::I32(0, ByteOrder::BigEndian).byte_len(), 4);
        assert_eq!(BinaryField::F32(0.0, ByteOrder::BigEndian).byte_len(), 4);
        assert_eq!(BinaryField::U64(0, ByteOrder::BigEndian).byte_len(), 8);
        assert_eq!(BinaryField::I64(0, ByteOrder::BigEndian).byte_len(), 8);
        assert_eq!(BinaryField::F64(0.0, ByteOrder::BigEndian).byte_len(), 8);
    }

    #[test]
    fn byte_len_raw() {
        assert_eq!(BinaryField::RawBytes(vec![1, 2, 3]).byte_len(), 3);
        assert_eq!(BinaryField::RawBytes(vec![]).byte_len(), 0);
    }

    // ── u8 / i8 ──────────────────────────────────────────────────────────────

    #[test]
    fn encode_u8() {
        assert_eq!(BinaryField::U8(0xAB).encode(), vec![0xAB]);
    }

    #[test]
    fn encode_i8_negative() {
        assert_eq!(BinaryField::I8(-1).encode(), vec![0xFF]);
    }

    // ── u16 ──────────────────────────────────────────────────────────────────

    #[test]
    fn encode_u16_be() {
        assert_eq!(
            BinaryField::U16(0x1234, ByteOrder::BigEndian).encode(),
            vec![0x12, 0x34]
        );
    }

    #[test]
    fn encode_u16_le() {
        assert_eq!(
            BinaryField::U16(0x1234, ByteOrder::LittleEndian).encode(),
            vec![0x34, 0x12]
        );
    }

    // ── u24 ──────────────────────────────────────────────────────────────────

    #[test]
    fn encode_u24_be() {
        // 0x123456 big-endian → [0x12, 0x34, 0x56]
        assert_eq!(
            BinaryField::U24(0x123456, ByteOrder::BigEndian).encode(),
            vec![0x12, 0x34, 0x56]
        );
    }

    #[test]
    fn encode_u24_le() {
        // 0x123456 little-endian → [0x56, 0x34, 0x12]
        assert_eq!(
            BinaryField::U24(0x123456, ByteOrder::LittleEndian).encode(),
            vec![0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn encode_u24_max() {
        // All 24 bits set
        assert_eq!(
            BinaryField::U24(0xFFFFFF, ByteOrder::BigEndian).encode(),
            vec![0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn encode_u24_zero() {
        assert_eq!(
            BinaryField::U24(0, ByteOrder::BigEndian).encode(),
            vec![0x00, 0x00, 0x00]
        );
    }

    // ── u32 ──────────────────────────────────────────────────────────────────

    #[test]
    fn encode_u32_be() {
        assert_eq!(
            BinaryField::U32(0xDEADBEEF, ByteOrder::BigEndian).encode(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn encode_u32_le() {
        assert_eq!(
            BinaryField::U32(0xDEADBEEF, ByteOrder::LittleEndian).encode(),
            vec![0xEF, 0xBE, 0xAD, 0xDE]
        );
    }

    // ── i32 ──────────────────────────────────────────────────────────────────

    #[test]
    fn encode_i32_negative_be() {
        assert_eq!(
            BinaryField::I32(-1, ByteOrder::BigEndian).encode(),
            vec![0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    // ── f32 / f64 ────────────────────────────────────────────────────────────

    #[test]
    fn encode_f32_be_known_value() {
        // 1.0f32 IEEE 754 big-endian = 0x3F800000
        assert_eq!(
            BinaryField::F32(1.0, ByteOrder::BigEndian).encode(),
            vec![0x3F, 0x80, 0x00, 0x00]
        );
    }

    #[test]
    fn encode_f64_be_known_value() {
        // 1.0f64 IEEE 754 big-endian = 0x3FF0000000000000
        assert_eq!(
            BinaryField::F64(1.0, ByteOrder::BigEndian).encode(),
            vec![0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    // ── RawBytes ─────────────────────────────────────────────────────────────

    #[test]
    fn encode_raw_bytes() {
        let bytes = vec![0x01, 0x02, 0x03];
        assert_eq!(BinaryField::RawBytes(bytes.clone()).encode(), bytes);
    }

    #[test]
    fn encode_raw_bytes_empty() {
        assert_eq!(BinaryField::RawBytes(vec![]).encode(), Vec::<u8>::new());
    }

    // ── encode_into accumulates correctly ─────────────────────────────────────

    #[test]
    fn encode_into_appends_to_existing_buffer() {
        let mut buf = vec![0xAA];
        BinaryField::U8(0xBB).encode_into(&mut buf);
        BinaryField::U16(0x0102, ByteOrder::BigEndian).encode_into(&mut buf);
        assert_eq!(buf, vec![0xAA, 0xBB, 0x01, 0x02]);
    }

    // ── Sea-Bird 911+ sample words ────────────────────────────────────────────

    #[test]
    fn sbe911_pressure_word_24bit_be() {
        // Representative Sea-Bird 911+ pressure sensor word: 3-byte big-endian unsigned int.
        let raw_counts: u32 = 0x7B_A2_C1;
        let encoded = BinaryField::U24(raw_counts, ByteOrder::BigEndian).encode();
        assert_eq!(encoded, vec![0x7B, 0xA2, 0xC1]);
        assert_eq!(encoded.len(), 3);
    }

    #[test]
    fn sbe911_temperature_word_24bit_be() {
        let raw_counts: u32 = 0x00_FF_AA;
        let encoded = BinaryField::U24(raw_counts, ByteOrder::BigEndian).encode();
        assert_eq!(encoded, vec![0x00, 0xFF, 0xAA]);
    }
}
