use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ByteOrder {
    #[default]
    BigEndian,
    LittleEndian,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_big_endian() {
        assert_eq!(ByteOrder::default(), ByteOrder::BigEndian);
    }
}
