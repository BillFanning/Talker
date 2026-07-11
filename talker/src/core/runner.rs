//! The per-channel send loop shared by the CLI and the GUI.
//!
//! The loop is business logic, so it lives in core (spec §2.2) — `cli` and
//! `gui` only wire channels to it. One [`run`] call owns one interface and
//! its schedule and executes until a [`TalkerCommand::Stop`] arrives or every
//! command sender is dropped.
//!
//! Waiting uses deadline-bounded blocking receives on the command channel
//! (ADR-002): a due message fires on time (no sleep-slice jitter), a command
//! is handled the moment it arrives, and an idle channel consumes no CPU.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};

use crate::core::{
    channel::{Interface, InterfaceConfig},
    scheduler::{Schedule, Tick},
    timing,
};

/// A command sent from the owning thread (UI or CLI) to a channel's runner.
pub enum TalkerCommand {
    Stop,
    /// Reopen the channel's interface with a new configuration.
    UpdateInterface(InterfaceConfig),
    /// Change message `index`'s send interval, effective immediately.
    SetInterval {
        index: usize,
        interval_ms: u64,
    },
}

/// A status update from a channel's runner.
///
/// Every variant names its `channel` (0-based index), so statuses stay
/// self-describing even when several channels share one status receiver —
/// the CLI funnels all channels into a single channel; the GUI keeps one
/// receiver per channel and ignores the field.
pub enum TalkerStatus {
    /// A message was sent. Carries both per-channel and per-message counts
    /// plus the wire bytes (for the display pane).
    Sent {
        channel: usize,
        /// Which message in the channel's schedule fired.
        message_index: usize,
        /// Running send count for this *specific message*.
        message_count: u64,
        /// Running send count across all messages in this channel.
        total_count: u64,
        /// Cumulative wire bytes sent across all messages in this channel.
        total_bytes: u64,
        /// Cumulative count of status updates this channel discarded because
        /// the receiver was full. Riding in every `Sent`, the reader
        /// self-corrects even when some updates (including ones carrying
        /// this field) were themselves dropped.
        dropped_statuses: u64,
        /// Cumulative count of sends skipped under the scheduler's stall
        /// policy (`Schedule::missed_sends`) — the channel couldn't keep to
        /// its configured cadence. Like the counts, rides in every `Sent`.
        missed_sends: u64,
        /// Exact bytes put on the wire.
        payload: Vec<u8>,
    },
    /// A send (or interface update) failed. **Edge-triggered** for sends: only
    /// the *first* failure of a failing episode is reported; repeats are
    /// counted, not re-reported, and [`SendRecovered`](Self::SendRecovered)
    /// closes the episode with the totals.
    ConnectionError { channel: usize, message: String },
    /// Sending resumed after a failing episode. Carries the episode's cost so
    /// the observer can state what was lost: `failures` sends were attempted
    /// and failed (the first was reported as `ConnectionError`), `suppressed`
    /// due fires were skipped by the bounded-backoff retry policy without
    /// being attempted at all.
    SendRecovered {
        channel: usize,
        failures: u64,
        suppressed: u64,
    },
    /// Opening the interface failed; the runner exits after sending this.
    OpenFailed { channel: usize, message: String },
}

/// First retry delay after a send failure (the **bounded-backoff** retry
/// policy): while an interface is failing, due fires are suppressed — counted,
/// not attempted — until the next retry instant; each failed retry doubles the
/// wait up to [`RETRY_BACKOFF_MAX`], and the first success closes the episode.
/// Without this, a 100 Hz schedule against a dead TCP/serial target retries
/// (and used to log) 100 times a second, burying the original failure.
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Retry delay cap: a persistently dead interface is probed at most once per
/// this interval. Recovery stays automatic — no manual Retry state (pinned by
/// `send_failure_reports_connection_error_and_keeps_running`).
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// One failing episode: from the first failed send (reported) to the first
/// successful one (reported with these counts). See [`RETRY_BACKOFF_INITIAL`].
struct FailureEpisode {
    /// Sends attempted and failed, ≥ 1 (the reported first one).
    failures: u64,
    /// Due fires suppressed by the backoff gate without an attempt.
    suppressed: u64,
    backoff: Duration,
    next_attempt: Instant,
}

/// The owning side's handle for a running talker thread.
pub struct TalkerHandle {
    pub cmd_tx: Sender<TalkerCommand>,
    pub status_rx: Receiver<TalkerStatus>,
    pub thread: std::thread::JoinHandle<()>,
}

/// Called after each status is queued, so an event-driven owner (the GUI)
/// can wake and drain instead of polling. Kept as a plain closure — core
/// stays UI-framework-free; the GUI passes `ctx.request_repaint`.
pub type StatusNotify = Box<dyn Fn() + Send>;

/// Open `cfg`'s interface, then run the send loop.
///
/// Meant to be called *on the channel's own thread* (the GUI path), so the
/// open — real I/O that can block for seconds on a TCP connect — never runs
/// on the UI thread. A failed open is reported as
/// [`TalkerStatus::OpenFailed`] and the call returns.
pub fn open_and_run(
    channel: usize,
    cfg: InterfaceConfig,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    status_tx: Sender<TalkerStatus>,
    notify: Option<StatusNotify>,
) {
    match cfg.open() {
        Ok(interface) => run(channel, interface, schedule, cmd_rx, status_tx, notify),
        Err(e) => {
            tracing::error!(
                channel = channel + 1,
                "failed to open channel {}: {e:#}",
                channel + 1
            );
            if status_tx
                .try_send(TalkerStatus::OpenFailed {
                    channel,
                    message: format!("{e:#}"),
                })
                .is_ok()
            {
                if let Some(n) = &notify {
                    n();
                }
            }
        }
    }
}

/// Run one channel's send loop until [`TalkerCommand::Stop`] arrives or the
/// command channel disconnects (the owning handle was dropped).
///
/// Log lines use the 1-based "channel N" form to match the UI labels.
pub fn run(
    channel: usize,
    interface: Box<dyn Interface>,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    status_tx: Sender<TalkerStatus>,
    notify: Option<StatusNotify>,
) {
    tracing::info!(
        channel = channel + 1,
        "channel {} running ({}-message schedule)",
        channel + 1,
        schedule.len()
    );
    run_loop(channel, interface, schedule, cmd_rx, status_tx, notify);
    tracing::info!(channel = channel + 1, "channel {} stopped", channel + 1);
}

enum Flow {
    Continue,
    Stop,
}

fn run_loop(
    channel: usize,
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    status_tx: Sender<TalkerStatus>,
    notify: Option<StatusNotify>,
) {
    let mut total_count = 0u64;
    let mut total_bytes = 0u64;
    // Per-message send counts, indexed by the message's position in the
    // compiled schedule. The resize is defensive; the schedule's size is
    // fixed at compile time.
    let mut per_message_counts: Vec<u64> = vec![0; schedule.len()];
    let mut dropped_statuses = 0u64;

    let handle = |cmd: TalkerCommand,
                  interface: &mut Box<dyn Interface>,
                  schedule: &mut Schedule,
                  episode: &mut Option<FailureEpisode>|
     -> Flow {
        match cmd {
            TalkerCommand::Stop => Flow::Stop,
            TalkerCommand::UpdateInterface(cfg) => {
                match cfg.open() {
                    Ok(new) => {
                        *interface = new;
                        // A fresh interface deserves an immediate attempt:
                        // pull the next retry forward. The episode's counts
                        // stay — only a successful send closes it (and
                        // reports what was lost).
                        if let Some(ep) = episode.as_mut() {
                            ep.next_attempt = Instant::now();
                        }
                        tracing::info!(
                            channel = channel + 1,
                            "channel {} interface updated",
                            channel + 1
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel = channel + 1,
                            "channel {} interface update failed: {e:#}",
                            channel + 1
                        );
                        let sent = status_tx
                            .try_send(TalkerStatus::ConnectionError {
                                channel,
                                message: format!("{e:#}"),
                            })
                            .is_ok();
                        if sent {
                            if let Some(n) = &notify {
                                n();
                            }
                        }
                    }
                }
                Flow::Continue
            }
            TalkerCommand::SetInterval { index, interval_ms } => {
                schedule.set_interval(index, interval_ms, Instant::now());
                Flow::Continue
            }
        }
    };

    // Hold the OS high-resolution timer exactly while the schedule needs it
    // (an interval below the threshold makes 15.625 ms deadline wakes skip
    // grid points — ADR-017). Re-evaluated every pass, so a SetInterval can
    // raise or release it mid-run; dropped with the runner either way.
    let mut timer_guard: Option<timing::HighResolutionGuard> = None;

    // The current failing episode, if any (bounded-backoff retry policy —
    // see [`RETRY_BACKOFF_INITIAL`]). `None` while sends are succeeding.
    let mut episode: Option<FailureEpisode> = None;

    loop {
        let fast = schedule
            .min_active_interval()
            .is_some_and(|i| i < timing::HIGH_RATE_THRESHOLD);
        if fast != timer_guard.is_some() {
            timer_guard = fast.then(timing::high_resolution);
        }

        // Drain anything already queued so back-to-back sends can't starve
        // command handling.
        for cmd in cmd_rx.try_iter() {
            if let Flow::Stop = handle(cmd, &mut interface, &mut schedule, &mut episode) {
                return;
            }
        }

        match schedule.poll(Instant::now()) {
            Tick::Send { index, payload } => {
                if let Some(ep) = episode.as_mut() {
                    if Instant::now() < ep.next_attempt {
                        // Backoff gate: this due fire is suppressed — counted,
                        // not attempted. The scheduler has already advanced,
                        // consistent with the stall policy (cadence over count).
                        ep.suppressed += 1;
                        continue;
                    }
                }
                match interface.send(&payload) {
                    Ok(()) => {
                        if let Some(ep) = episode.take() {
                            tracing::info!(
                            channel = channel + 1,
                            "channel {} sending recovered after {} failed and {} suppressed sends",
                            channel + 1,
                            ep.failures,
                            ep.suppressed
                        );
                            if status_tx
                                .try_send(TalkerStatus::SendRecovered {
                                    channel,
                                    failures: ep.failures,
                                    suppressed: ep.suppressed,
                                })
                                .is_ok()
                            {
                                if let Some(n) = &notify {
                                    n();
                                }
                            }
                        }
                        total_count += 1;
                        total_bytes += payload.len() as u64;
                        if index >= per_message_counts.len() {
                            per_message_counts.resize(index + 1, 0);
                        }
                        per_message_counts[index] += 1;
                        let status = TalkerStatus::Sent {
                            channel,
                            message_index: index,
                            message_count: per_message_counts[index],
                            total_count,
                            total_bytes,
                            dropped_statuses,
                            missed_sends: schedule.missed_sends(),
                            payload,
                        };
                        // Best-effort: a full receiver drops the update rather
                        // than backpressuring the send cadence — counts
                        // self-correct via the next delivered status.
                        match status_tx.try_send(status) {
                            Ok(()) => {
                                if let Some(n) = &notify {
                                    n();
                                }
                            }
                            Err(TrySendError::Full(_)) => {
                                dropped_statuses += 1;
                                if dropped_statuses == 1 {
                                    tracing::warn!(
                                        channel = channel + 1,
                                        "channel {}: status receiver is falling behind — sends \
                                     continue at cadence; display updates are being sampled",
                                        channel + 1
                                    );
                                }
                            }
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                    Err(e) => match episode.as_mut() {
                        // Edge-triggered: only the episode's first failure is
                        // reported (warn + `ConnectionError`); it opens the episode.
                        None => {
                            tracing::warn!(
                                channel = channel + 1,
                                "channel {} send failed (retrying with backoff): {e:#}",
                                channel + 1
                            );
                            episode = Some(FailureEpisode {
                                failures: 1,
                                suppressed: 0,
                                backoff: RETRY_BACKOFF_INITIAL,
                                next_attempt: Instant::now() + RETRY_BACKOFF_INITIAL,
                            });
                            let sent = status_tx
                                .try_send(TalkerStatus::ConnectionError {
                                    channel,
                                    message: format!("{e:#}"),
                                })
                                .is_ok();
                            if sent {
                                if let Some(n) = &notify {
                                    n();
                                }
                            }
                        }
                        // A failed retry deepens the backoff; no re-report.
                        Some(ep) => {
                            ep.failures += 1;
                            ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                            ep.next_attempt = Instant::now() + ep.backoff;
                            tracing::debug!(
                                channel = channel + 1,
                                "channel {} send still failing ({} failures so far): {e:#}",
                                channel + 1,
                                ep.failures
                            );
                        }
                    },
                }
            }
            // Nothing due yet: block on the command channel until the next
            // fire deadline. Wakes instantly for a command, exactly on time
            // for the schedule, and detects a dropped handle.
            Tick::Wait(until) => match cmd_rx.recv_deadline(until) {
                Ok(cmd) => {
                    if let Flow::Stop = handle(cmd, &mut interface, &mut schedule, &mut episode) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            },
            // No active messages: nothing can happen until a command arrives,
            // so block indefinitely — zero wakeups.
            Tick::Idle => match cmd_rx.recv() {
                Ok(cmd) => {
                    if let Flow::Stop = handle(cmd, &mut interface, &mut schedule, &mut episode) {
                        return;
                    }
                }
                Err(_) => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::core::channel::TcpClientConfig;
    use crate::core::message::{MessageConfig, PayloadConfig};

    /// An [`Interface`] that records every payload (or fails on demand).
    struct MockInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: bool,
    }

    impl Interface for MockInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail, "mock send failure");
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn spawn_runner(
        messages: &[MessageConfig],
        fail: bool,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(MockInterface {
            sent: Arc::clone(&sent),
            fail,
        });
        let schedule = Schedule::compile(messages, Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let thread =
            std::thread::spawn(move || run(0, interface, schedule, cmd_rx, status_tx, None));
        (
            sent,
            TalkerHandle {
                cmd_tx,
                status_rx,
                thread,
            },
        )
    }

    /// Wait (bounded) for the runner thread to finish.
    fn join_within(handle: TalkerHandle, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !handle.thread.is_finished() {
            assert!(Instant::now() < deadline, "runner did not stop in time");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.thread.join().unwrap();
    }

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    #[test]
    fn sends_on_schedule_and_reports_self_describing_counts() {
        let (sent, handle) = spawn_runner(&[msg("AB", 10)], false);
        // Let a few fires happen, then stop.
        std::thread::sleep(Duration::from_millis(60));
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let statuses: Vec<TalkerStatus> = handle.status_rx.try_iter().collect();
        join_within(handle, Duration::from_secs(2));

        let payloads = sent.lock().unwrap();
        assert!(
            payloads.len() >= 3,
            "expected ≥3 sends, got {}",
            payloads.len()
        );
        assert!(payloads.iter().all(|p| p == &vec![0xAB]));

        // Statuses carry identity and monotonically increasing counts.
        let mut last_total = 0;
        for s in &statuses {
            match s {
                TalkerStatus::Sent {
                    channel,
                    message_index,
                    total_count,
                    total_bytes,
                    dropped_statuses,
                    ..
                } => {
                    assert_eq!(*channel, 0);
                    assert_eq!(*message_index, 0);
                    assert!(*total_count > last_total);
                    last_total = *total_count;
                    // Every payload is the single byte 0xAB, so the byte
                    // total tracks the send count exactly.
                    assert_eq!(*total_bytes, *total_count);
                    assert_eq!(*dropped_statuses, 0);
                }
                _ => panic!("unexpected non-Sent status"),
            }
        }
        assert_eq!(last_total as usize, payloads.len());
    }

    #[test]
    fn stop_is_prompt_even_when_idle() {
        // All-dormant schedule → the runner blocks on the command channel.
        let (_, handle) = spawn_runner(&[msg("AB", 0)], false);
        std::thread::sleep(Duration::from_millis(20));
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn dropped_command_handle_stops_the_runner() {
        let (_, handle) = spawn_runner(&[msg("AB", 0)], false);
        let TalkerHandle {
            cmd_tx,
            status_rx: _status_rx,
            thread,
        } = handle;
        drop(cmd_tx); // owner went away without Stop
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "runner leaked after disconnect");
            std::thread::sleep(Duration::from_millis(5));
        }
        thread.join().unwrap();
    }

    #[test]
    fn send_failure_reports_connection_error_and_keeps_running() {
        let (_, handle) = spawn_runner(&[msg("AB", 10)], true);
        std::thread::sleep(Duration::from_millis(40));
        let mut saw_error = false;
        for s in handle.status_rx.try_iter() {
            if let TalkerStatus::ConnectionError { channel, message } = s {
                assert_eq!(channel, 0);
                assert!(message.contains("mock send failure"));
                saw_error = true;
            }
        }
        assert!(saw_error, "expected at least one ConnectionError");
        assert!(
            !handle.thread.is_finished(),
            "runner must survive send errors"
        );
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    #[test]
    fn repeated_send_failures_report_one_connection_error() {
        // 5 ms fires against a permanently failing interface: without the
        // edge trigger this would be ~20 ConnectionErrors in 100 ms; with it,
        // exactly one (the retry backoff starts at 250 ms, so no second
        // attempt happens inside the window).
        let (_, handle) = spawn_runner(&[msg("AB", 5)], true);
        std::thread::sleep(Duration::from_millis(100));
        let errors = handle
            .status_rx
            .try_iter()
            .filter(|s| matches!(s, TalkerStatus::ConnectionError { .. }))
            .count();
        assert_eq!(
            errors, 1,
            "edge-triggered: only the episode's first failure is reported"
        );
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    /// An [`Interface`] whose failure mode can be flipped at runtime — for the
    /// recovery path.
    struct FlakyInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Interface for FlakyInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(
                !self.fail.load(std::sync::atomic::Ordering::SeqCst),
                "mock send failure"
            );
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    #[test]
    fn recovery_reports_send_recovered_with_episode_counts() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(FlakyInterface {
            sent: Arc::clone(&sent),
            fail: Arc::clone(&fail),
        });
        let schedule = Schedule::compile(&[msg("AB", 5)], Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let thread =
            std::thread::spawn(move || run(0, interface, schedule, cmd_rx, status_tx, None));
        let handle = TalkerHandle {
            cmd_tx,
            status_rx,
            thread,
        };

        // Let the episode open (first failure) and some fires get suppressed,
        // then heal the interface: the next backoff retry closes the episode.
        std::thread::sleep(Duration::from_millis(50));
        fail.store(false, std::sync::atomic::Ordering::SeqCst);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut recovered = None;
        while recovered.is_none() {
            assert!(
                Instant::now() < deadline,
                "expected a SendRecovered after the interface healed"
            );
            for s in handle.status_rx.try_iter() {
                if let TalkerStatus::SendRecovered {
                    failures,
                    suppressed,
                    ..
                } = s
                {
                    recovered = Some((failures, suppressed));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (failures, suppressed) = recovered.unwrap();
        assert!(failures >= 1, "the reported first failure is counted");
        assert!(
            suppressed >= 1,
            "5 ms fires during the 250 ms backoff are suppressed, not attempted"
        );
        assert!(
            !sent.lock().unwrap().is_empty(),
            "sending resumed after recovery"
        );
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
    }

    #[test]
    fn open_and_run_reports_open_failed() {
        // Port 1 on loopback refuses immediately on every platform we target.
        let cfg = InterfaceConfig::TcpClient(TcpClientConfig::new("127.0.0.1:1".parse().unwrap()));
        let schedule = Schedule::compile(&[msg("AB", 100)], Instant::now()).unwrap();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::bounded::<TalkerCommand>(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        open_and_run(3, cfg, schedule, cmd_rx, status_tx, None);
        match status_rx.try_recv() {
            Ok(TalkerStatus::OpenFailed { channel, message }) => {
                assert_eq!(channel, 3);
                assert!(message.contains("127.0.0.1:1"), "message was: {message}");
            }
            other => panic!("expected OpenFailed, got {:?}", other.is_ok()),
        }
    }
}
