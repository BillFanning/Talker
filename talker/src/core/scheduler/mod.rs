//! The per-channel scheduler.
//!
//! Each message in a channel fires on its own independent interval. The
//! scheduler tracks a next-fire time per message and, on each [`poll`], hands
//! the talker loop the message that is due earliest (ties broken by message
//! order). A message whose interval is zero is *dormant*: it is kept so its
//! interval can later be changed, but it never fires.

use std::time::{Duration, Instant};

use anyhow::Context;

use crate::core::message::MessageConfig;

/// One compiled message tracked by the scheduler.
#[derive(Debug)]
struct ScheduledMessage {
    payload: Vec<u8>,
    /// Send interval. Zero means the message is dormant.
    interval: Duration,
    /// When this message should next fire (only meaningful while active).
    next_fire: Instant,
}

impl ScheduledMessage {
    fn is_active(&self) -> bool {
        !self.interval.is_zero()
    }
}

/// What the talker loop should do next, returned by [`Schedule::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    /// Send `payload` now; it belongs to the message at `index`.
    Send { index: usize, payload: Vec<u8> },
    /// Nothing is due yet — sleep until at most this instant.
    Wait(Instant),
    /// No active messages; nothing fires until an interval is changed.
    Idle,
}

/// A channel's messages, scheduled by next-fire time.
#[derive(Debug)]
pub struct Schedule {
    messages: Vec<ScheduledMessage>,
}

impl Schedule {
    /// Compile a channel's messages into a runnable schedule.
    ///
    /// `start` is the reference instant: every active message is scheduled to
    /// fire at `start` (i.e. immediately). Returns an error if `messages` is
    /// empty or a payload fails to compile.
    pub fn compile(messages: &[MessageConfig], start: Instant) -> anyhow::Result<Self> {
        anyhow::ensure!(!messages.is_empty(), "channel has no messages");
        let messages = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let payload = m
                    .payload
                    .compile()
                    .with_context(|| format!("compiling message {i}"))?;
                Ok(ScheduledMessage {
                    payload,
                    interval: Duration::from_millis(m.interval_ms),
                    next_fire: start,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self { messages })
    }

    /// Index of the active message with the earliest next-fire time.
    ///
    /// Ties are broken by message order (lowest index first).
    fn earliest(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_active())
            .min_by_key(|(i, m)| (m.next_fire, *i))
            .map(|(i, _)| i)
    }

    /// Decide what to do at `now`.
    ///
    /// If a message is due, its next-fire time is advanced by its interval
    /// (from the previous fire time, so the cadence does not drift) and the
    /// payload is returned for sending. Otherwise reports how long to wait.
    pub fn poll(&mut self, now: Instant) -> Tick {
        let Some(index) = self.earliest() else {
            return Tick::Idle;
        };
        let msg = &mut self.messages[index];
        if msg.next_fire <= now {
            msg.next_fire += msg.interval;
            Tick::Send {
                index,
                payload: msg.payload.clone(),
            }
        } else {
            Tick::Wait(msg.next_fire)
        }
    }

    /// Change message `index`'s send interval, effective immediately.
    ///
    /// An interval of 0 makes the message dormant. A non-zero interval
    /// (re)schedules it to fire at `now + interval`. An out-of-range index is
    /// ignored.
    pub fn set_interval(&mut self, index: usize, interval_ms: u64, now: Instant) {
        if let Some(msg) = self.messages.get_mut(index) {
            msg.interval = Duration::from_millis(interval_ms);
            if msg.is_active() {
                msg.next_fire = now + msg.interval;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::message::PayloadConfig;

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ── compile ───────────────────────────────────────────────────────────────

    #[test]
    fn compile_empty_returns_error() {
        assert!(Schedule::compile(&[], Instant::now()).is_err());
    }

    #[test]
    fn compile_bad_payload_returns_error_with_context() {
        let err = Schedule::compile(&[msg("XYZ", 100)], Instant::now()).unwrap_err();
        assert!(err.to_string().contains("message 0"));
    }

    // ── poll ──────────────────────────────────────────────────────────────────

    #[test]
    fn single_message_fires_immediately_then_waits() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        match s.poll(t0) {
            Tick::Send { index, payload } => {
                assert_eq!(index, 0);
                assert_eq!(payload, vec![0xAB]);
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
        assert_eq!(s.poll(t0 + ms(99)), Tick::Wait(t0 + ms(100)));
        assert!(matches!(s.poll(t0 + ms(100)), Tick::Send { index: 0, .. }));
    }

    #[test]
    fn independent_intervals_fire_at_their_own_rate() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("01", 100), msg("02", 300)], t0).unwrap();
        // both due at t0; tie broken by message order
        assert!(matches!(s.poll(t0), Tick::Send { index: 0, .. }));
        assert!(matches!(s.poll(t0), Tick::Send { index: 1, .. }));
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
        // the 100 ms message fires at 100 and 200 on its own
        assert!(matches!(s.poll(t0 + ms(100)), Tick::Send { index: 0, .. }));
        assert!(matches!(s.poll(t0 + ms(200)), Tick::Send { index: 0, .. }));
        // at 300 both are due again; message order decides
        assert!(matches!(s.poll(t0 + ms(300)), Tick::Send { index: 0, .. }));
        assert!(matches!(s.poll(t0 + ms(300)), Tick::Send { index: 1, .. }));
    }

    #[test]
    fn next_fire_does_not_drift_when_polled_late() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        assert!(matches!(s.poll(t0), Tick::Send { .. })); // next fire -> t0+100
                                                          // polled late at t0+150: still due; next fire advances from 100, not 150
        assert!(matches!(s.poll(t0 + ms(150)), Tick::Send { .. }));
        assert_eq!(s.poll(t0 + ms(150)), Tick::Wait(t0 + ms(200)));
    }

    #[test]
    fn nmea_payload_compiles_and_fires() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(
            &[MessageConfig::new(
                PayloadConfig::nmea("GP", "GGA", vec![]),
                1000,
            )],
            t0,
        )
        .unwrap();
        match s.poll(t0) {
            Tick::Send { payload, .. } => {
                let wire = std::str::from_utf8(&payload).unwrap();
                assert!(wire.starts_with("$GPGGA*"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    // ── dormant messages ──────────────────────────────────────────────────────

    #[test]
    fn dormant_message_never_fires() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("00", 0), msg("11", 100)], t0).unwrap();
        // only the active message fires
        assert!(matches!(s.poll(t0), Tick::Send { index: 1, .. }));
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
    }

    #[test]
    fn all_dormant_is_idle() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("00", 0), msg("11", 0)], t0).unwrap();
        assert_eq!(s.poll(t0), Tick::Idle);
    }

    // ── set_interval ──────────────────────────────────────────────────────────

    #[test]
    fn set_interval_to_zero_makes_dormant() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        s.set_interval(0, 0, t0);
        assert_eq!(s.poll(t0), Tick::Idle);
    }

    #[test]
    fn set_interval_reschedules_from_now() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        s.set_interval(0, 200, t0 + ms(50));
        assert_eq!(s.poll(t0 + ms(50)), Tick::Wait(t0 + ms(250)));
        assert!(matches!(s.poll(t0 + ms(250)), Tick::Send { index: 0, .. }));
    }

    #[test]
    fn set_interval_can_revive_a_dormant_message() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 0)], t0).unwrap();
        assert_eq!(s.poll(t0), Tick::Idle);
        s.set_interval(0, 100, t0);
        assert_eq!(s.poll(t0), Tick::Wait(t0 + ms(100)));
    }

    #[test]
    fn set_interval_out_of_range_is_ignored() {
        let t0 = Instant::now();
        let mut s = Schedule::compile(&[msg("AB", 100)], t0).unwrap();
        s.set_interval(99, 500, t0); // must not panic
        assert!(matches!(s.poll(t0), Tick::Send { index: 0, .. }));
    }
}
