mod config;

pub use config::{PayloadConfig, ScheduleConfig, ScheduleEntryConfig};

use std::time::Duration;

use anyhow::Context;

/// A compiled, wire-ready payload paired with how long to wait after sending it.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    pub payload: Vec<u8>,
    pub interval: Duration,
}

/// A cycling list of compiled schedule entries.
///
/// Call [`next_entry`][Schedule::next_entry] to get the current entry and
/// advance the cursor; it wraps back to the first entry after the last.
#[derive(Debug)]
pub struct Schedule {
    entries: Vec<ScheduleEntry>,
    cursor: usize,
}

impl Schedule {
    pub fn new(entries: Vec<ScheduleEntry>) -> anyhow::Result<Self> {
        anyhow::ensure!(!entries.is_empty(), "schedule must have at least one entry");
        Ok(Self { entries, cursor: 0 })
    }

    /// Return a reference to the current entry and advance the cursor.
    pub fn next_entry(&mut self) -> &ScheduleEntry {
        let entry = &self.entries[self.cursor];
        self.cursor = (self.cursor + 1) % self.entries.len();
        entry
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ScheduleConfig {
    /// Compile all entries into a runnable [`Schedule`].
    pub fn compile(&self) -> anyhow::Result<Schedule> {
        anyhow::ensure!(!self.entries.is_empty(), "schedule config has no entries");
        let entries = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let payload = e
                    .payload
                    .compile()
                    .with_context(|| format!("compiling schedule entry {i}"))?;
                Ok(ScheduleEntry {
                    payload,
                    interval: Duration::from_millis(e.interval_ms),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Schedule::new(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hex: &str, ms: u64) -> ScheduleEntryConfig {
        ScheduleEntryConfig::new(PayloadConfig::raw_hex(hex), ms)
    }

    // ── Schedule::new ─────────────────────────────────────────────────────────

    #[test]
    fn new_with_empty_entries_returns_error() {
        assert!(Schedule::new(vec![]).is_err());
    }

    #[test]
    fn new_with_one_entry_succeeds() {
        let s = Schedule::new(vec![ScheduleEntry {
            payload: vec![0x01],
            interval: Duration::from_millis(100),
        }])
        .unwrap();
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
    }

    // ── Schedule::next_entry ──────────────────────────────────────────────────

    #[test]
    fn next_entry_advances_cursor() {
        let mut s = Schedule::new(vec![
            ScheduleEntry {
                payload: vec![1],
                interval: Duration::from_millis(100),
            },
            ScheduleEntry {
                payload: vec![2],
                interval: Duration::from_millis(200),
            },
        ])
        .unwrap();

        assert_eq!(s.next_entry().payload, vec![1]);
        assert_eq!(s.next_entry().payload, vec![2]);
    }

    #[test]
    fn next_entry_wraps_to_start() {
        let mut s = Schedule::new(vec![
            ScheduleEntry {
                payload: vec![1],
                interval: Duration::from_millis(100),
            },
            ScheduleEntry {
                payload: vec![2],
                interval: Duration::from_millis(200),
            },
        ])
        .unwrap();

        s.next_entry(); // 1
        s.next_entry(); // 2
        assert_eq!(s.next_entry().payload, vec![1]); // wraps
    }

    #[test]
    fn next_entry_single_entry_always_returns_same() {
        let mut s = Schedule::new(vec![ScheduleEntry {
            payload: vec![0xFF],
            interval: Duration::from_millis(500),
        }])
        .unwrap();
        for _ in 0..5 {
            assert_eq!(s.next_entry().payload, vec![0xFF]);
        }
    }

    #[test]
    fn next_entry_interval_matches_config() {
        let mut s = Schedule::new(vec![ScheduleEntry {
            payload: vec![0],
            interval: Duration::from_millis(750),
        }])
        .unwrap();
        assert_eq!(s.next_entry().interval, Duration::from_millis(750));
    }

    // ── ScheduleConfig::compile ───────────────────────────────────────────────

    #[test]
    fn compile_empty_config_returns_error() {
        let cfg = ScheduleConfig::new(vec![]);
        assert!(cfg.compile().is_err());
    }

    #[test]
    fn compile_single_raw_hex_entry() {
        let cfg = ScheduleConfig::new(vec![entry("AABB", 100)]);
        let mut sched = cfg.compile().unwrap();
        assert_eq!(sched.len(), 1);
        assert_eq!(sched.next_entry().payload, vec![0xAA, 0xBB]);
        assert_eq!(sched.next_entry().interval, Duration::from_millis(100));
    }

    #[test]
    fn compile_multiple_entries_preserves_order() {
        let cfg = ScheduleConfig::new(vec![entry("01", 100), entry("02", 200), entry("03", 300)]);
        let mut sched = cfg.compile().unwrap();
        assert_eq!(sched.next_entry().payload, vec![0x01]);
        assert_eq!(sched.next_entry().payload, vec![0x02]);
        assert_eq!(sched.next_entry().payload, vec![0x03]);
        // wraps
        assert_eq!(sched.next_entry().payload, vec![0x01]);
    }

    #[test]
    fn compile_nmea_entry() {
        let cfg = ScheduleConfig::new(vec![ScheduleEntryConfig::new(
            PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string()]),
            1000,
        )]);
        let mut sched = cfg.compile().unwrap();
        let wire = std::str::from_utf8(&sched.next_entry().payload).unwrap();
        assert!(wire.starts_with("$GPGGA,123519*"));
        assert!(wire.ends_with("\r\n"));
    }

    #[test]
    fn compile_bad_payload_returns_error_with_context() {
        let cfg = ScheduleConfig::new(vec![entry("XYZ", 100)]);
        let err = cfg.compile().unwrap_err();
        assert!(err.to_string().contains("entry 0"));
    }
}
