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

use std::time::Instant;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};

use crate::core::{
    channel::{Interface, InterfaceConfig},
    scheduler::{Schedule, Tick},
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
        /// Cumulative count of status updates this channel discarded because
        /// the receiver was full. Riding in every `Sent`, the reader
        /// self-corrects even when some updates (including ones carrying
        /// this field) were themselves dropped.
        dropped_statuses: u64,
        /// Exact bytes put on the wire.
        payload: Vec<u8>,
    },
    ConnectionError {
        channel: usize,
        message: String,
    },
    /// Opening the interface failed; the runner exits after sending this.
    OpenFailed {
        channel: usize,
        message: String,
    },
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
            tracing::error!("failed to open channel {}: {e:#}", channel + 1);
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
        "channel {} running ({}-message schedule)",
        channel + 1,
        schedule.len()
    );
    run_loop(channel, interface, schedule, cmd_rx, status_tx, notify);
    tracing::info!("channel {} stopped", channel + 1);
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
    // Per-message send counts, indexed by the message's position in the
    // compiled schedule. The resize is defensive; the schedule's size is
    // fixed at compile time.
    let mut per_message_counts: Vec<u64> = vec![0; schedule.len()];
    let mut dropped_statuses = 0u64;

    let handle = |cmd: TalkerCommand,
                  interface: &mut Box<dyn Interface>,
                  schedule: &mut Schedule|
     -> Flow {
        match cmd {
            TalkerCommand::Stop => Flow::Stop,
            TalkerCommand::UpdateInterface(cfg) => {
                match cfg.open() {
                    Ok(new) => {
                        *interface = new;
                        tracing::info!("channel {} interface updated", channel + 1);
                    }
                    Err(e) => {
                        tracing::warn!("channel {} interface update failed: {e:#}", channel + 1);
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

    loop {
        // Drain anything already queued so back-to-back sends can't starve
        // command handling.
        for cmd in cmd_rx.try_iter() {
            if let Flow::Stop = handle(cmd, &mut interface, &mut schedule) {
                return;
            }
        }

        match schedule.poll(Instant::now()) {
            Tick::Send { index, payload } => match interface.send(&payload) {
                Ok(()) => {
                    total_count += 1;
                    if index >= per_message_counts.len() {
                        per_message_counts.resize(index + 1, 0);
                    }
                    per_message_counts[index] += 1;
                    let status = TalkerStatus::Sent {
                        channel,
                        message_index: index,
                        message_count: per_message_counts[index],
                        total_count,
                        dropped_statuses,
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
                                    "channel {}: status receiver is falling behind — sends \
                                     continue at cadence; display updates are being sampled",
                                    channel + 1
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => {}
                    }
                }
                Err(e) => {
                    tracing::warn!("channel {} send failed: {e:#}", channel + 1);
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
            },
            // Nothing due yet: block on the command channel until the next
            // fire deadline. Wakes instantly for a command, exactly on time
            // for the schedule, and detects a dropped handle.
            Tick::Wait(until) => match cmd_rx.recv_deadline(until) {
                Ok(cmd) => {
                    if let Flow::Stop = handle(cmd, &mut interface, &mut schedule) {
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
                    if let Flow::Stop = handle(cmd, &mut interface, &mut schedule) {
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
                    dropped_statuses,
                    ..
                } => {
                    assert_eq!(*channel, 0);
                    assert_eq!(*message_index, 0);
                    assert!(*total_count > last_total);
                    last_total = *total_count;
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
