//! Per-sink subsampling (spec §50.1, §164).
//!
//! A [`Subsampler`] decides, per Message, whether that Message passes to a sink
//! (a display view, or a message-oriented recording). It is a
//! **presentation/recording filter only** — it never affects reception, Message
//! Numbering, retention, or any other sink (§50.1, §99/§100). Subsampled sinks
//! therefore show **gapped, not renumbered** Message Numbers (§89). Raw byte data
//! (`.dat`) is never subsampled.

use std::time::{Duration, Instant};

use crate::config::Subsample;

/// Stateful per-sink subsampler. One per sink (each display view / recording has
/// its own), so sinks subsample independently.
pub struct Subsampler {
    policy: Subsample,
    seen: u64,
    last_pass: Option<Instant>,
}

impl Subsampler {
    pub fn new(policy: Subsample) -> Self {
        Self {
            policy,
            seen: 0,
            last_pass: None,
        }
    }

    /// Whether the Message arriving at `at` passes to this sink.
    pub fn should_pass(&mut self, at: Instant) -> bool {
        match self.policy {
            Subsample::None => true,
            Subsample::EveryNth { n } => {
                // `n == 0` is rejected at config validation; guard to 1 anyway.
                let n = (n as u64).max(1);
                let pass = self.seen.is_multiple_of(n);
                self.seen += 1;
                pass
            }
            Subsample::RateLimit { millis } => {
                let interval = Duration::from_millis(millis);
                match self.last_pass {
                    Some(t) if at.saturating_duration_since(t) < interval => false,
                    _ => {
                        self.last_pass = Some(at);
                        true
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn none_passes_everything() {
        let mut s = Subsampler::new(Subsample::None);
        let t = Instant::now();
        assert!((0..5).all(|_| s.should_pass(t)));
    }

    #[test]
    fn every_nth_passes_one_in_n_from_the_first() {
        let mut s = Subsampler::new(Subsample::EveryNth { n: 3 });
        let t = Instant::now();
        let passes: Vec<bool> = (0..7).map(|_| s.should_pass(t)).collect();
        assert_eq!(passes, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn rate_limit_passes_at_most_one_per_interval() {
        let base = Instant::now();
        let mut s = Subsampler::new(Subsample::RateLimit { millis: 100 });
        assert!(s.should_pass(base)); // first always passes
        assert!(!s.should_pass(base + ms(50))); // within the interval
        assert!(s.should_pass(base + ms(150))); // interval elapsed
        assert!(!s.should_pass(base + ms(160)));
        assert!(s.should_pass(base + ms(300)));
    }
}
