//! Pure string/number formatting helpers for GUI readouts. No egui, no state.

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
}
