//! NMEA UTC time-field formatting.
//!
//! The inputs mirror Chrono's `Timelike` convention: `second` is the whole non-leap
//! second (0..=59), while `subsec_millis` may be 1000..=1999 when an instant
//! represents the inserted leap second. Keeping that normalization here lets
//! every NMEA producer emit the same fixed-width field without depending on a
//! particular clock crate.

/// Format an NMEA UTC time field as `hhmmss` or `hhmmss.sss`.
///
/// `subsec_millis` is milliseconds since the whole non-leap second. Its normal
/// range is 0..=999; 1000..=1999 is accepted only with `second == 59` and is
/// rendered as leap second `60` with the excess milliseconds. Callers are
/// responsible for supplying valid clock components.
pub fn format_utc_time(
    hour: u32,
    minute: u32,
    second: u32,
    subsec_millis: u32,
    include_millis: bool,
) -> String {
    debug_assert!(hour <= 23);
    debug_assert!(minute <= 59);
    debug_assert!(second <= 59);
    debug_assert!(subsec_millis <= 1_999);
    debug_assert!(subsec_millis < 1_000 || second == 59);

    let leap_second = subsec_millis / 1_000;
    let displayed_second = second + leap_second;
    let millis = subsec_millis % 1_000;

    if include_millis {
        format!("{hour:02}{minute:02}{displayed_second:02}.{millis:03}")
    } else {
        format!("{hour:02}{minute:02}{displayed_second:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_fixed_width_time_with_optional_milliseconds() {
        assert_eq!(format_utc_time(1, 2, 3, 45, false), "010203");
        assert_eq!(format_utc_time(1, 2, 3, 45, true), "010203.045");
    }

    #[test]
    fn chrono_style_leap_second_stays_fixed_width_and_truthful() {
        assert_eq!(format_utc_time(23, 59, 59, 1_800, false), "235960");
        assert_eq!(format_utc_time(23, 59, 59, 1_800, true), "235960.800");
    }
}
