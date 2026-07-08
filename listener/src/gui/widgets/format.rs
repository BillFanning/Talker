//! Pure string/number formatting helpers for the GUI readouts. No egui, no state.

/// Format a byte count compactly in SI units (kB = 1000 B, MB = 1000 kB, …) for the
/// stream liveness readouts (ADR-009): the Channel list rows and the detail header.
pub(crate) fn human_bytes(n: u64) -> String {
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
