/// Compute NMEA 0183 XOR checksum over the payload bytes (everything between `$` and `*`).
pub fn xor(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc ^ b)
}

/// Parse a two-character hex checksum suffix like `*47` → `0x47`.
pub fn from_hex(s: &str) -> Option<u8> {
    let hex = s.strip_prefix('*')?;
    if hex.len() != 2 {
        return None;
    }
    u8::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_basic() {
        // $GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47
        let payload = b"GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";
        assert_eq!(xor(payload), 0x47);
    }

    #[test]
    fn xor_empty() {
        assert_eq!(xor(b""), 0x00);
    }

    #[test]
    fn xor_single_byte() {
        assert_eq!(xor(b"A"), b'A');
    }

    #[test]
    fn xor_two_same_bytes() {
        assert_eq!(xor(b"AA"), 0x00);
    }

    #[test]
    fn from_hex_valid() {
        assert_eq!(from_hex("*47"), Some(0x47));
        assert_eq!(from_hex("*00"), Some(0x00));
        assert_eq!(from_hex("*FF"), Some(0xFF));
    }

    #[test]
    fn from_hex_lowercase() {
        assert_eq!(from_hex("*4a"), Some(0x4a));
    }

    #[test]
    fn from_hex_no_prefix() {
        assert_eq!(from_hex("47"), None);
    }

    #[test]
    fn from_hex_too_short() {
        assert_eq!(from_hex("*4"), None);
    }

    #[test]
    fn from_hex_invalid_chars() {
        assert_eq!(from_hex("*GG"), None);
    }
}
