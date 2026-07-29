//! Pure string/number formatting helpers for GUI readouts. No egui, no state.

use std::time::Duration;

/// Format a byte count compactly in SI units (kB = 1000 B, MB = 1000 kB, …)
/// for liveness readouts: channel-list rows and detail headers.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.3} {}", UNITS[u])
    }
}

/// Format a byte-per-second rate in the same SI units as [`human_bytes`], so a
/// readout showing a total beside a rate scales both the same way.
///
/// One decimal throughout, including zero: a stopped channel reads `0.0 B/s`
/// rather than dropping the segment, so the row keeps its shape at rest.
pub fn human_byte_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B/s", "kB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes_per_sec.max(0.0);
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Format a duration compactly for timing telemetry readouts.
pub fn compact_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        return format!("{nanos} ns");
    }
    if nanos < 1_000_000 {
        return scaled_duration(nanos as f64 / 1_000.0, "us");
    }
    if nanos < 1_000_000_000 {
        return scaled_duration(nanos as f64 / 1_000_000.0, "ms");
    }
    scaled_duration(duration.as_secs_f64(), "s")
}

fn scaled_duration(value: f64, unit: &str) -> String {
    if value < 10.0 {
        format!("{value:.2} {unit}")
    } else if value < 100.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_below_1k_are_plain() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
    }

    #[test]
    fn si_units_scale_by_1000() {
        assert_eq!(human_bytes(1000), "1.000 kB");
        assert_eq!(human_bytes(1_500_000), "1.500 MB");
        assert_eq!(human_bytes(2_000_000_000), "2.000 GB");
    }

    #[test]
    fn byte_rates_scale_like_totals_and_keep_a_decimal_at_rest() {
        assert_eq!(human_byte_rate(0.0), "0.0 B/s");
        assert_eq!(human_byte_rate(999.0), "999.0 B/s");
        assert_eq!(human_byte_rate(1_000.0), "1.0 kB/s");
        assert_eq!(human_byte_rate(12_400.0), "12.4 kB/s");
        assert_eq!(human_byte_rate(1_500_000.0), "1.5 MB/s");
        // A negative rate is not meaningful; clamp rather than print "-0.0".
        assert_eq!(human_byte_rate(-5.0), "0.0 B/s");
    }

    #[test]
    fn durations_choose_compact_units_and_precision() {
        assert_eq!(compact_duration(Duration::ZERO), "0 ns");
        assert_eq!(compact_duration(Duration::from_nanos(999)), "999 ns");
        assert_eq!(compact_duration(Duration::from_micros(1)), "1.00 us");
        assert_eq!(compact_duration(Duration::from_micros(12)), "12.0 us");
        assert_eq!(compact_duration(Duration::from_micros(999)), "999 us");
        assert_eq!(compact_duration(Duration::from_micros(1_250)), "1.25 ms");
        assert_eq!(compact_duration(Duration::from_millis(15)), "15.0 ms");
        assert_eq!(compact_duration(Duration::from_millis(999)), "999 ms");
        assert_eq!(compact_duration(Duration::from_millis(1_250)), "1.25 s");
    }
}
