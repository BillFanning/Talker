//! Runtime events, warnings, errors, and diagnostic logging (spec §91–§95,
//! §114–§118).
//!
//! This is `listener-diagnostics` (§128). It owns the diagnostic record model
//! (Events §92, Warnings §93, Errors §94), bounded per-type history
//! ([`DiagnosticLog`], §86/§88 — each severity count-capped, oldest evicted),
//! and diagnostic logging ([`init_logging`], §114).

use std::time::SystemTime;

use crate::retention::{CountBounded, DEFAULT_BACKSTOP};

/// Severity of a diagnostic, ordered low → high priority (§92–§95).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum DiagnosticSeverity {
    /// Something happened (§92: channel started, client connected, …).
    Event,
    /// May affect operation but does not prevent it (§93).
    Warning,
    /// Failure to complete or continue an operation (§94).
    Error,
}

/// A diagnostic record retained for review (§91–§95). `timestamp` is the wall-clock
/// time the record was created (millisecond display precision, §26-style).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub timestamp: SystemTime,
}

impl Diagnostic {
    pub fn new(severity: DiagnosticSeverity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            timestamp: SystemTime::now(),
        }
    }

    /// Construct with an explicit timestamp (for deterministic tests / replay).
    pub fn at(
        severity: DiagnosticSeverity,
        message: impl Into<String>,
        timestamp: SystemTime,
    ) -> Self {
        Self {
            severity,
            message: message.into(),
            timestamp,
        }
    }

    pub fn event(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Event, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(DiagnosticSeverity::Error, message)
    }
}

/// Bounded diagnostic history (§86, §88). Events, Warnings, and Errors are
/// retained **separately**, each count-limited (§88, from `RetentionConfig`'s
/// `event_limit`/`warning_limit`/`error_limit`). Eviction is oldest-first (§89);
/// [`clear`](Self::clear) discards all retained diagnostics (§90). An unset limit
/// falls back to the hard backstop so memory stays bounded (§80, §124).
pub struct DiagnosticLog {
    events: CountBounded<Diagnostic>,
    warnings: CountBounded<Diagnostic>,
    errors: CountBounded<Diagnostic>,
    revision: u64,
}

impl DiagnosticLog {
    pub fn new(
        event_limit: Option<usize>,
        warning_limit: Option<usize>,
        error_limit: Option<usize>,
    ) -> Self {
        let cap = |limit: Option<usize>| limit.unwrap_or(DEFAULT_BACKSTOP);
        Self {
            events: CountBounded::new(cap(event_limit)),
            warnings: CountBounded::new(cap(warning_limit)),
            errors: CountBounded::new(cap(error_limit)),
            revision: 0,
        }
    }

    /// How many times this log's contents have changed.
    ///
    /// A poll-driven consumer rebuilds its view only when this moves. The log is
    /// polled far more often than it is written — a quiet channel is still polled
    /// several times a second — so the cheap comparison saves cloning every
    /// retained entry on every poll (§124).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Record a diagnostic into the store for its severity (§88).
    pub fn record(&mut self, diagnostic: Diagnostic) {
        match diagnostic.severity {
            DiagnosticSeverity::Event => self.events.push(diagnostic),
            DiagnosticSeverity::Warning => self.warnings.push(diagnostic),
            DiagnosticSeverity::Error => self.errors.push(diagnostic),
        }
        // Eviction changes the contents too, so every push is a new revision.
        self.revision = self.revision.saturating_add(1);
    }

    pub fn events(&self) -> impl Iterator<Item = &Diagnostic> {
        self.events.iter()
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.warnings.iter()
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.errors.iter()
    }

    /// Discard all retained diagnostics (§90).
    pub fn clear(&mut self) {
        self.events.clear();
        self.warnings.clear();
        self.errors.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Seed this (fresh) log with prior diagnostics, so a restarted Channel keeps the
    /// previous run's log instead of starting blank (§88, within-session). The entries
    /// are replayed in chronological order through `record`, so the per-severity caps
    /// still bound the result (oldest dropped). Intended to be called once, on a freshly
    /// constructed log.
    pub fn seed(&mut self, mut prior: Vec<Diagnostic>) {
        prior.sort_by_key(|d| d.timestamp);
        for d in prior {
            self.record(d);
        }
    }
}

/// Initialize diagnostic logging (§114): a `tracing` subscriber filtered by the
/// `RUST_LOG` environment variable (defaulting to `info`). Logging failure is
/// non-fatal (§117): returns whether the global subscriber was installed (it can
/// only be installed once per process). Persistent log files / rotation are
/// deferred (§118, Appendix A).
pub fn init_logging() -> bool {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_routes_by_severity() {
        let mut log = DiagnosticLog::new(None, None, None);
        log.record(Diagnostic::event("started"));
        log.record(Diagnostic::warning("checksum invalid"));
        log.record(Diagnostic::warning("queue overflow"));
        log.record(Diagnostic::error("port not found"));

        assert_eq!(log.events().count(), 1);
        assert_eq!(log.warnings().count(), 2);
        assert_eq!(log.errors().count(), 1);
    }

    #[test]
    fn per_type_limits_evict_oldest() {
        // Keep at most one warning; events/errors generous.
        let mut log = DiagnosticLog::new(None, Some(1), None);
        log.record(Diagnostic::warning("first"));
        log.record(Diagnostic::warning("second"));
        let warnings: Vec<&str> = log.warnings().map(|d| d.message.as_str()).collect();
        assert_eq!(warnings, vec!["second"]); // oldest evicted (§89)
    }

    #[test]
    fn clear_discards_all() {
        let mut log = DiagnosticLog::new(None, None, None);
        log.record(Diagnostic::event("e"));
        log.record(Diagnostic::error("x"));
        log.clear();
        assert_eq!(log.events().count(), 0);
        assert_eq!(log.errors().count(), 0);
    }

    #[test]
    fn severity_orders_low_to_high() {
        assert!(DiagnosticSeverity::Event < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Error);
    }

    #[test]
    fn diagnostics_carry_a_timestamp() {
        use std::time::{Duration, SystemTime};
        // Constructed records stamp "now"; `at` lets tests pin an exact time so a
        // merged log can be ordered chronologically across severities.
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_millis(250);
        let a = Diagnostic::at(DiagnosticSeverity::Event, "first", t0);
        let b = Diagnostic::at(DiagnosticSeverity::Error, "second", t1);
        assert!(a.timestamp < b.timestamp);

        let fresh = Diagnostic::event("now");
        assert!(fresh.timestamp <= SystemTime::now());
    }
}
