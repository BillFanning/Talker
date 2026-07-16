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
    channel::{ChannelId, Interface, InterfaceConfig},
    scheduler::{Schedule, Tick},
    timing,
};

/// Who a runner is, fixed at start (ADR-020): the stable [`ChannelId`] every
/// status and structured log field carries (attribution that survives slot
/// shifts), and the human label used in log *text* ("channel 3 …",
/// "channel 'GPS' …"). The label is frozen for the run — a rename shows up
/// on the next start; the id is what routing trusts.
#[derive(Clone, Debug)]
pub struct RunnerIdentity {
    pub id: ChannelId,
    pub label: String,
}

/// Process-unique identity of a live control command. The id lets observers
/// correlate an enqueue attempt with the runner's eventual execution result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u64);

impl CommandId {
    pub fn mint() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The independently recoverable control target a command mutates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CommandTarget {
    Stop,
    Interface,
    MessageInterval(usize),
}

/// What the runner did with an enqueued command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandExecution {
    Applied,
    Failed(String),
}

/// Reliable runner-to-owner control state. This uses a dedicated bounded lane,
/// separate from drop-and-count telemetry: configuration truth must not disappear
/// merely because the sampled observer queue is full.
#[derive(Clone, Debug)]
pub enum RunnerControlStatus {
    /// The runner successfully opened its start-time interface.
    InterfaceOpened {
        channel: ChannelId,
        config: InterfaceConfig,
    },
    /// One enqueued live mutation finished executing.
    CommandCompleted {
        channel: ChannelId,
        id: CommandId,
        target: CommandTarget,
        execution: CommandExecution,
    },
}

/// A command sent from the owning thread (UI or CLI) to a channel's runner.
pub enum TalkerCommand {
    Stop,
    /// Reopen the channel's interface with a new configuration.
    UpdateInterface {
        id: CommandId,
        config: InterfaceConfig,
    },
    /// Change message `index`'s send interval, effective immediately.
    SetInterval {
        id: CommandId,
        index: usize,
        interval_ms: u64,
    },
}

/// A status update from a channel's runner.
///
/// Every variant names its channel by stable [`ChannelId`] (ADR-020), so
/// statuses stay self-describing even when several channels share one status
/// receiver — the CLI funnels all channels into a single channel; the
/// supervisor keeps one receiver per slot and routes by slot. A slot index
/// would go stale the moment a channel above is removed; the id never does.
pub enum TalkerStatus {
    /// Periodic counters (ADR-018 lane 1): cumulative totals, **no payload**.
    /// Emitted at most once per [`ObserverPolicy::counter_interval`] on the
    /// send path, plus once when the runner stops (so totals are exact at
    /// rest). Every field is cumulative, so the reader self-corrects even
    /// when some updates were dropped by a full queue.
    Counters {
        channel: ChannelId,
        /// Running send count across all messages in this channel.
        total_count: u64,
        /// Cumulative wire bytes sent across all messages in this channel.
        total_bytes: u64,
        /// Per-message running send counts, indexed by the message's
        /// position in the compiled schedule.
        per_message_counts: Vec<u64>,
        /// Cumulative count of status updates this channel discarded because
        /// the receiver was full.
        dropped_statuses: u64,
        /// Cumulative count of sends skipped under the scheduler's stall
        /// policy (`Schedule::missed_sends`) — the channel couldn't keep to
        /// its configured cadence.
        missed_sends: u64,
        /// Cumulative send calls that reached the interface and failed.
        failed_sends: u64,
        /// Cumulative due fires suppressed by the bounded-backoff gate.
        suppressed_sends: u64,
    },
    /// A sampled send (ADR-018 lane 2): the exact wire bytes of one send,
    /// for the Output pane. Newest-per-interval — the first send after
    /// [`ObserverPolicy::sample_interval`] elapses carries its payload — so
    /// the pane shows a live, bounded sample rather than every message.
    /// `ObserverPolicy::every_send` (CLI `--echo`) makes this every send.
    SendSample {
        channel: ChannelId,
        /// Which message in the channel's schedule fired.
        message_index: usize,
        /// Exact bytes put on the wire.
        payload: Vec<u8>,
        /// Byte positions in `payload` produced by lossy code-page fallback.
        /// Literal `?` bytes are deliberately absent.
        replacement_wire_offsets: Vec<usize>,
    },
    /// A send failed. **Edge-triggered**: only
    /// the *first* failure of a failing episode is reported; repeats are
    /// counted, not re-reported, and [`SendRecovered`](Self::SendRecovered)
    /// closes the episode with the totals.
    ConnectionError { channel: ChannelId, message: String },
    /// Sending resumed after a failing episode. Carries the episode's cost so
    /// the observer can state what was lost: `failures` sends were attempted
    /// and failed (the first was reported as `ConnectionError`), `suppressed`
    /// due fires were skipped by the bounded-backoff retry policy without
    /// being attempted at all.
    SendRecovered {
        channel: ChannelId,
        failures: u64,
        suppressed: u64,
    },
    /// Opening the interface failed; the runner exits after sending this.
    OpenFailed { channel: ChannelId, message: String },
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

/// How a runner reports to its observer (ADR-018): the named policy the owner
/// passes to [`run`]/[`open_and_run`]. Counters and payload samples are
/// **rate-limited lanes**; errors ([`TalkerStatus::ConnectionError`] /
/// [`TalkerStatus::SendRecovered`] / [`TalkerStatus::OpenFailed`]) are always
/// immediate and never rate-limited.
#[derive(Clone, Copy, Debug)]
pub struct ObserverPolicy {
    /// Minimum spacing between [`TalkerStatus::Counters`] emissions. A final
    /// one is always emitted when the runner stops.
    pub counter_interval: Duration,
    /// Minimum spacing between payload-bearing [`TalkerStatus::SendSample`]s.
    /// `Duration::ZERO` = every send carries its payload (CLI `--echo`).
    pub sample_interval: Duration,
}

impl ObserverPolicy {
    /// The GUI default: ~5 Hz counters, ~10 Hz payload samples — display cost
    /// stays constant regardless of send rate (ADR-018).
    pub fn sampled() -> Self {
        Self {
            counter_interval: Duration::from_millis(200),
            sample_interval: Duration::from_millis(100),
        }
    }

    /// Every send emits its payload (CLI `--echo` — the one consumer that
    /// genuinely wants every wire message). Counters stay periodic.
    pub fn every_send() -> Self {
        Self {
            sample_interval: Duration::ZERO,
            ..Self::sampled()
        }
    }
}

impl Default for ObserverPolicy {
    fn default() -> Self {
        Self::sampled()
    }
}

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
    pub control_rx: Receiver<RunnerControlStatus>,
    pub status_rx: Receiver<TalkerStatus>,
    pub thread: std::thread::JoinHandle<()>,
}

/// Called after each status is queued, so an event-driven owner (the GUI)
/// can wake and drain instead of polling. Kept as a plain closure — core
/// stays UI-framework-free; the GUI passes `ctx.request_repaint`.
pub type StatusNotify = Box<dyn Fn() + Send>;

/// Runner-to-owner reporting endpoints and cadence policy. Keeping this wiring
/// together prevents entry points from growing parallel positional arguments as
/// observer and reliable-control lanes evolve.
pub struct RunnerObserver {
    control_tx: Option<Sender<RunnerControlStatus>>,
    status_tx: Sender<TalkerStatus>,
    notify: Option<StatusNotify>,
    policy: ObserverPolicy,
}

impl RunnerObserver {
    pub fn new(status_tx: Sender<TalkerStatus>, policy: ObserverPolicy) -> Self {
        Self {
            control_tx: None,
            status_tx,
            notify: None,
            policy,
        }
    }

    pub fn with_control(mut self, control_tx: Sender<RunnerControlStatus>) -> Self {
        self.control_tx = Some(control_tx);
        self
    }

    pub fn with_notify(mut self, notify: StatusNotify) -> Self {
        self.notify = Some(notify);
        self
    }
}

/// Open `cfg`'s interface, then run the send loop.
///
/// Meant to be called *on the channel's own thread* (the GUI path), so the
/// open — real I/O that can block for seconds on a TCP connect — never runs
/// on the UI thread. A failed open is reported as
/// [`TalkerStatus::OpenFailed`] and the call returns.
pub fn open_and_run(
    who: RunnerIdentity,
    cfg: InterfaceConfig,
    schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    match cfg.open() {
        Ok(interface) => {
            emit_control(
                &observer.control_tx,
                &observer.notify,
                RunnerControlStatus::InterfaceOpened {
                    channel: who.id,
                    config: cfg.clone(),
                },
            );
            run(who, interface, Some(cfg), schedule, cmd_rx, observer);
        }
        Err(e) => {
            tracing::error!(
                channel = who.id.as_u64(),
                "failed to open channel {}: {e:#}",
                who.label
            );
            if observer
                .status_tx
                .try_send(TalkerStatus::OpenFailed {
                    channel: who.id,
                    message: format!("{e:#}"),
                })
                .is_ok()
            {
                if let Some(n) = &observer.notify {
                    n();
                }
            }
        }
    }
}

/// Run one channel's send loop until [`TalkerCommand::Stop`] arrives or the
/// command channel disconnects (the owning handle was dropped).
///
/// Log text names the channel by `who.label`; the structured `channel` field
/// carries the stable id (ADR-020).
pub fn run(
    who: RunnerIdentity,
    interface: Box<dyn Interface>,
    current_config: Option<InterfaceConfig>,
    mut schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    // Cadence starts only now: any profile preflight, predecessor join, TCP
    // connect, or serial open happened before this runner boundary and must not
    // inflate missed-send telemetry or shift the first-fire grid.
    schedule.arm(Instant::now());
    tracing::info!(
        channel = who.id.as_u64(),
        "channel {} running ({}-message schedule)",
        who.label,
        schedule.len()
    );
    run_loop(&who, interface, current_config, schedule, cmd_rx, observer);
    tracing::info!(channel = who.id.as_u64(), "channel {} stopped", who.label);
}

enum Flow {
    Continue,
    Stop,
}

fn run_loop(
    who: &RunnerIdentity,
    mut interface: Box<dyn Interface>,
    mut current_config: Option<InterfaceConfig>,
    mut schedule: Schedule,
    cmd_rx: Receiver<TalkerCommand>,
    observer: RunnerObserver,
) {
    let RunnerObserver {
        control_tx,
        status_tx,
        notify,
        policy,
    } = observer;
    let mut total_count = 0u64;
    let mut total_bytes = 0u64;
    let mut failed_sends = 0u64;
    let mut suppressed_sends = 0u64;
    // Lane rate limits (ADR-018): `None` = nothing emitted yet, so the first
    // send always produces both a sample and counters (instant first paint).
    let mut last_sample: Option<Instant> = None;
    let mut last_counters: Option<Instant> = None;
    // Sample rotation: the scheduler breaks grid ties by lowest index, so an
    // aligned multi-message schedule would sample message 0 forever if the
    // lane just took the first due send. Skip a repeat of the last sampled
    // message while due — but never longer than one full cycle, so a
    // single-active-message schedule still samples.
    let mut last_sampled_index: Option<usize> = None;
    let mut repeats_skipped_while_due = 0u64;
    // Per-message send counts, indexed by the message's position in the
    // compiled schedule. The resize is defensive; the schedule's size is
    // fixed at compile time.
    let mut per_message_counts: Vec<u64> = vec![0; schedule.len()];
    let mut dropped_statuses = 0u64;

    let handle = |cmd: TalkerCommand,
                  interface: &mut Box<dyn Interface>,
                  current_config: &mut Option<InterfaceConfig>,
                  schedule: &mut Schedule,
                  episode: &mut Option<FailureEpisode>|
     -> Flow {
        match cmd {
            TalkerCommand::Stop => Flow::Stop,
            TalkerCommand::UpdateInterface { id, config } => {
                let execution = match current_config.as_ref() {
                    Some(current) => match interface.reconfigure(current, &config) {
                        Ok(true) => CommandExecution::Applied,
                        Ok(false) => match config.open() {
                            Ok(new) => {
                                *interface = new;
                                CommandExecution::Applied
                            }
                            Err(e) => CommandExecution::Failed(format!("{e:#}")),
                        },
                        Err(e) => CommandExecution::Failed(format!("{e:#}")),
                    },
                    None => match config.open() {
                        Ok(new) => {
                            *interface = new;
                            CommandExecution::Applied
                        }
                        Err(e) => CommandExecution::Failed(format!("{e:#}")),
                    },
                };
                match &execution {
                    CommandExecution::Applied => {
                        *current_config = Some(config);
                        // A fresh interface deserves an immediate attempt:
                        // pull the next retry forward. The episode's counts
                        // stay — only a successful send closes it (and
                        // reports what was lost).
                        if let Some(ep) = episode.as_mut() {
                            ep.next_attempt = Instant::now();
                        }
                        tracing::info!(
                            channel = who.id.as_u64(),
                            "channel {} interface updated",
                            who.label
                        );
                    }
                    CommandExecution::Failed(message) => {
                        tracing::warn!(
                            channel = who.id.as_u64(),
                            "channel {} interface update failed: {message}",
                            who.label,
                        );
                    }
                }
                emit_control(
                    &control_tx,
                    &notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: who.id,
                        id,
                        target: CommandTarget::Interface,
                        execution,
                    },
                );
                Flow::Continue
            }
            TalkerCommand::SetInterval {
                id,
                index,
                interval_ms,
            } => {
                let execution = if schedule.set_interval(index, interval_ms, Instant::now()) {
                    CommandExecution::Applied
                } else {
                    CommandExecution::Failed(format!(
                        "message index {index} is outside the {}-message schedule",
                        schedule.len()
                    ))
                };
                emit_control(
                    &control_tx,
                    &notify,
                    RunnerControlStatus::CommandCompleted {
                        channel: who.id,
                        id,
                        target: CommandTarget::MessageInterval(index),
                        execution,
                    },
                );
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

    'run: loop {
        let fast = schedule
            .min_active_interval()
            .is_some_and(|i| i < timing::HIGH_RATE_THRESHOLD);
        if fast != timer_guard.is_some() {
            timer_guard = fast.then(timing::high_resolution);
        }

        // Drain anything already queued so back-to-back sends can't starve
        // command handling.
        for cmd in cmd_rx.try_iter() {
            if let Flow::Stop = handle(
                cmd,
                &mut interface,
                &mut current_config,
                &mut schedule,
                &mut episode,
            ) {
                break 'run;
            }
        }

        match schedule.poll(Instant::now()) {
            Tick::Send { index, payload } => {
                let suppressed = episode
                    .as_ref()
                    .is_some_and(|ep| Instant::now() < ep.next_attempt);
                if suppressed {
                    // Backoff gate: this due fire is suppressed — counted,
                    // not attempted. The scheduler has already advanced,
                    // consistent with the stall policy (cadence over count).
                    if let Some(ep) = episode.as_mut() {
                        ep.suppressed += 1;
                    }
                    suppressed_sends += 1;
                } else {
                    match interface.send(&payload) {
                        Ok(()) => {
                            if let Some(ep) = episode.take() {
                                tracing::info!(
                            channel = who.id.as_u64(),
                            "channel {} sending recovered after {} failed and {} suppressed sends",
                            who.label,
                            ep.failures,
                            ep.suppressed
                        );
                                emit_status(
                                    &status_tx,
                                    &notify,
                                    who,
                                    &mut dropped_statuses,
                                    TalkerStatus::SendRecovered {
                                        channel: who.id,
                                        failures: ep.failures,
                                        suppressed: ep.suppressed,
                                    },
                                );
                            }
                            total_count += 1;
                            total_bytes += payload.len() as u64;
                            if index >= per_message_counts.len() {
                                per_message_counts.resize(index + 1, 0);
                            }
                            per_message_counts[index] += 1;
                            // The payload observer lane is rate-limited and
                            // best-effort (ADR-018): a full receiver drops the
                            // sample rather than backpressuring send cadence.
                            let now = Instant::now();
                            let due = last_sample.is_none_or(|t| now - t >= policy.sample_interval);
                            if due {
                                let repeat =
                                    last_sampled_index == Some(index) && schedule.len() > 1;
                                if !repeat || repeats_skipped_while_due >= schedule.len() as u64 {
                                    last_sample = Some(now);
                                    last_sampled_index = Some(index);
                                    repeats_skipped_while_due = 0;
                                    let replacement_wire_offsets =
                                        schedule.replacement_wire_offsets(index).to_vec();
                                    emit_status(
                                        &status_tx,
                                        &notify,
                                        who,
                                        &mut dropped_statuses,
                                        TalkerStatus::SendSample {
                                            channel: who.id,
                                            message_index: index,
                                            payload,
                                            replacement_wire_offsets,
                                        },
                                    );
                                } else {
                                    repeats_skipped_while_due += 1;
                                }
                            }
                        }
                        Err(e) => {
                            failed_sends += 1;
                            match episode.as_mut() {
                                // Edge-triggered: only the episode's first failure is
                                // reported (warn + `ConnectionError`); it opens the episode.
                                None => {
                                    tracing::warn!(
                                        channel = who.id.as_u64(),
                                        "channel {} send failed (retrying with backoff): {e:#}",
                                        who.label
                                    );
                                    episode = Some(FailureEpisode {
                                        failures: 1,
                                        suppressed: 0,
                                        backoff: RETRY_BACKOFF_INITIAL,
                                        next_attempt: Instant::now() + RETRY_BACKOFF_INITIAL,
                                    });
                                    emit_status(
                                        &status_tx,
                                        &notify,
                                        who,
                                        &mut dropped_statuses,
                                        TalkerStatus::ConnectionError {
                                            channel: who.id,
                                            message: format!("{e:#}"),
                                        },
                                    );
                                }
                                // A failed retry deepens the backoff; no re-report.
                                Some(ep) => {
                                    ep.failures += 1;
                                    ep.backoff = (ep.backoff * 2).min(RETRY_BACKOFF_MAX);
                                    ep.next_attempt = Instant::now() + ep.backoff;
                                    tracing::debug!(
                                        channel = who.id.as_u64(),
                                        "channel {} send still failing ({} failures so far): {e:#}",
                                        who.label,
                                        ep.failures
                                    );
                                }
                            }
                        }
                    }
                }

                // Cumulative outcomes remain observable even when every due
                // send is failing or suppressed. This lane is rate-limited
                // and best-effort, so it cannot slow the scheduler hot path.
                let now = Instant::now();
                if last_counters.is_none_or(|t| now - t >= policy.counter_interval) {
                    last_counters = Some(now);
                    let drops_so_far = dropped_statuses;
                    emit_status(
                        &status_tx,
                        &notify,
                        who,
                        &mut dropped_statuses,
                        TalkerStatus::Counters {
                            channel: who.id,
                            total_count,
                            total_bytes,
                            per_message_counts: per_message_counts.clone(),
                            dropped_statuses: drops_so_far,
                            missed_sends: schedule.missed_sends(),
                            failed_sends,
                            suppressed_sends,
                        },
                    );
                }
            }
            // Nothing due yet: block on the command channel until the next
            // fire deadline. Wakes instantly for a command, exactly on time
            // for the schedule, and detects a dropped handle.
            Tick::Wait(until) => match cmd_rx.recv_deadline(until) {
                Ok(cmd) => {
                    if let Flow::Stop = handle(
                        cmd,
                        &mut interface,
                        &mut current_config,
                        &mut schedule,
                        &mut episode,
                    ) {
                        break 'run;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break 'run,
            },
            // No active messages: nothing can happen until a command arrives,
            // so block indefinitely — zero wakeups.
            Tick::Idle => match cmd_rx.recv() {
                Ok(cmd) => {
                    if let Flow::Stop = handle(
                        cmd,
                        &mut interface,
                        &mut current_config,
                        &mut schedule,
                        &mut episode,
                    ) {
                        break 'run;
                    }
                }
                Err(_) => break 'run,
            },
        }
    }

    // Final counters (ADR-018): the rate-limited lane can be up to one
    // interval stale when the runner stops — emit once more so the observer's
    // totals are exact at rest. A **blocking** send, deliberately: the runner
    // is exiting so cadence no longer matters, and this is the one status
    // that must not be lost to a momentarily full queue ("exact at rest" is
    // a promise, not best-effort). The owner keeps draining a stopped
    // runner's receiver until the thread exits (supervisor `poll`/`join_all`,
    // the CLI's funnel loop), and a dropped receiver returns an error
    // immediately — so this cannot hang.
    let _ = status_tx.send(TalkerStatus::Counters {
        channel: who.id,
        total_count,
        total_bytes,
        per_message_counts,
        dropped_statuses,
        missed_sends: schedule.missed_sends(),
        failed_sends,
        suppressed_sends,
    });
    if let Some(n) = &notify {
        n();
    }
}

/// Queue one status update, best-effort (never blocks the send cadence): a
/// full receiver counts a drop (`dropped_statuses` — cumulative fields in the
/// next delivered `Counters` self-correct), a disconnected one is ignored.
fn emit_status(
    status_tx: &Sender<TalkerStatus>,
    notify: &Option<StatusNotify>,
    who: &RunnerIdentity,
    dropped_statuses: &mut u64,
    status: TalkerStatus,
) {
    match status_tx.try_send(status) {
        Ok(()) => {
            if let Some(n) = notify {
                n();
            }
        }
        Err(TrySendError::Full(_)) => {
            *dropped_statuses += 1;
            if *dropped_statuses == 1 {
                tracing::warn!(
                    channel = who.id.as_u64(),
                    "channel {}: status receiver is falling behind — sends continue at \
                     cadence; observer updates are being dropped and counted",
                    who.label
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Queue control truth reliably. Commands are rare and the control queue is sized
/// from the command queue, so an owner that keeps polling cannot lose a completion;
/// a dropped owner releases the send immediately with `Disconnected`.
fn emit_control(
    control_tx: &Option<Sender<RunnerControlStatus>>,
    notify: &Option<StatusNotify>,
    status: RunnerControlStatus,
) {
    let Some(control_tx) = control_tx else {
        return;
    };
    if control_tx.send(status).is_ok() {
        if let Some(n) = notify {
            n();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::core::channel::TcpClientConfig;
    use crate::core::message::{CodePage, MessageConfig, PayloadConfig};

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
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        // Tests default to the every-send policy so per-send behaviour stays
        // directly observable; the sampled lanes have their own test.
        spawn_runner_with(messages, fail, ObserverPolicy::every_send())
    }

    fn spawn_runner_with(
        messages: &[MessageConfig],
        fail: bool,
        policy: ObserverPolicy,
    ) -> (Arc<Mutex<Vec<Vec<u8>>>>, TalkerHandle, ChannelId) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let interface = Box::new(MockInterface {
            sent: Arc::clone(&sent),
            fail,
        });
        let schedule = Schedule::compile(messages, Instant::now()).unwrap();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (control_tx, control_rx) = crossbeam_channel::bounded(16);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "test".into(),
        };
        let id = who.id;
        let thread = std::thread::spawn(move || {
            run(
                who,
                interface,
                None,
                schedule,
                cmd_rx,
                RunnerObserver::new(status_tx, policy).with_control(control_tx),
            )
        });
        (
            sent,
            TalkerHandle {
                cmd_tx,
                control_rx,
                status_rx,
                thread,
            },
            id,
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
        let (sent, handle, id) = spawn_runner(&[msg("AB", 10)], false);
        // Wait (bounded) for a few fires rather than assuming a wall-clock
        // window: under the stall policy (skip the backlog, stay on grid) a
        // stalled CI VM can legitimately fire only once in a fixed 60 ms —
        // that's the policy working, not a defect (flaked on macOS CI).
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 3 {
            assert!(
                Instant::now() < deadline,
                "expected ≥3 sends within 2 s, got {}",
                sent.lock().unwrap().len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        // Collect statuses only after the runner has fully stopped — a send
        // can land between an early drain and the Stop being processed, which
        // would desync `last_total` from the payload count below.
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));
        let statuses: Vec<TalkerStatus> = status_rx.try_iter().collect();

        let payloads = sent.lock().unwrap();
        assert!(payloads.iter().all(|p| p == &vec![0xAB]));

        // Lanes carry identity; counters are monotonic and exact at rest
        // (the final Counters emitted at stop — ADR-018).
        let mut samples = 0usize;
        let mut last_total = 0u64;
        for s in &statuses {
            match s {
                TalkerStatus::SendSample {
                    channel,
                    message_index,
                    payload,
                    replacement_wire_offsets,
                } => {
                    assert_eq!(*channel, id, "samples carry the stable id");
                    assert_eq!(*message_index, 0);
                    assert_eq!(payload, &vec![0xAB]);
                    assert!(replacement_wire_offsets.is_empty());
                    samples += 1;
                }
                TalkerStatus::Counters {
                    channel,
                    total_count,
                    total_bytes,
                    per_message_counts,
                    dropped_statuses,
                    ..
                } => {
                    assert_eq!(*channel, id, "counters carry the stable id");
                    assert!(*total_count >= last_total, "counters must be monotonic");
                    last_total = *total_count;
                    // Every payload is the single byte 0xAB, so the byte
                    // total tracks the send count exactly.
                    assert_eq!(*total_bytes, *total_count);
                    assert_eq!(per_message_counts.iter().sum::<u64>(), *total_count);
                    assert_eq!(*dropped_statuses, 0);
                }
                _ => panic!("unexpected status variant"),
            }
        }
        // every-send policy: one sample per send; the final Counters makes
        // the totals exact.
        assert_eq!(samples, payloads.len());
        assert_eq!(last_total as usize, payloads.len());
    }

    #[test]
    fn send_sample_carries_code_page_replacement_provenance() {
        let message = MessageConfig::new(
            PayloadConfig::Ascii {
                text: "?—".to_string(),
                code_page: CodePage::Iso8859_1,
            },
            10,
        );
        let (sent, handle, _) = spawn_runner(&[message], false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "expected one send within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let sample = status_rx
            .try_iter()
            .find_map(|status| match status {
                TalkerStatus::SendSample {
                    payload,
                    replacement_wire_offsets,
                    ..
                } => Some((payload, replacement_wire_offsets)),
                _ => None,
            })
            .expect("runner should emit a payload sample");
        assert_eq!(sample.0, b"??");
        assert_eq!(sample.1, vec![1]);
    }

    /// The sample lane rotates across message indices: with an aligned
    /// two-message schedule the low-index tie-break used to sample message 0
    /// forever; every message must reach the Output pane.
    #[test]
    fn sample_lane_rotates_across_messages() {
        let policy = ObserverPolicy {
            counter_interval: Duration::from_secs(3600),
            sample_interval: Duration::from_millis(5),
        };
        let (sent, handle, _id) = spawn_runner_with(&[msg("AB", 5), msg("CD", 5)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 40 {
            assert!(Instant::now() < deadline, "expected sends within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let mut seen = [false; 2];
        for s in status_rx.try_iter() {
            if let TalkerStatus::SendSample { message_index, .. } = s {
                if message_index < 2 {
                    seen[message_index] = true;
                }
            }
        }
        assert!(
            seen[0] && seen[1],
            "both messages must be sampled (got 0: {}, 1: {})",
            seen[0],
            seen[1]
        );
    }

    #[test]
    fn sampled_policy_bounds_payload_traffic() {
        // A huge sample interval: only the *first* send carries its payload,
        // however many sends happen; a zero counter interval keeps totals
        // exact per send. Pins the ADR-018 claim that display cost is
        // decoupled from send rate.
        let policy = ObserverPolicy {
            counter_interval: Duration::ZERO,
            sample_interval: Duration::from_secs(3600),
        };
        let (sent, handle, _id) = spawn_runner_with(&[msg("AB", 5)], false, policy);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sent.lock().unwrap().len() < 3 {
            assert!(Instant::now() < deadline, "expected ≥3 sends within 2 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        let status_rx = handle.status_rx.clone();
        join_within(handle, Duration::from_secs(2));

        let mut samples = 0usize;
        let mut last_total = 0u64;
        for s in status_rx.try_iter() {
            match s {
                TalkerStatus::SendSample { .. } => samples += 1,
                TalkerStatus::Counters { total_count, .. } => last_total = total_count,
                _ => panic!("unexpected status variant"),
            }
        }
        assert_eq!(samples, 1, "one payload sample regardless of send count");
        assert_eq!(last_total as usize, sent.lock().unwrap().len());
    }

    #[test]
    fn stop_is_prompt_even_when_idle() {
        // All-dormant schedule → the runner blocks on the command channel.
        let (_, handle, _id) = spawn_runner(&[msg("AB", 0)], false);
        std::thread::sleep(Duration::from_millis(20));
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(1));
    }

    #[test]
    fn dropped_command_handle_stops_the_runner() {
        let (_, handle, _id) = spawn_runner(&[msg("AB", 0)], false);
        let TalkerHandle {
            cmd_tx,
            control_rx: _control_rx,
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
        let (_, handle, id) = spawn_runner(&[msg("AB", 10)], true);
        std::thread::sleep(Duration::from_millis(40));
        let mut saw_error = false;
        for s in handle.status_rx.try_iter() {
            if let TalkerStatus::ConnectionError { channel, message } = s {
                assert_eq!(channel, id, "errors carry the stable id");
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
        // edge trigger every retry could emit a ConnectionError; with it,
        // exactly one is emitted for the whole failure episode.
        let (_, handle, _id) = spawn_runner(&[msg("AB", 5)], true);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut statuses = Vec::new();
        loop {
            statuses.extend(handle.status_rx.try_iter());
            let saw_live_outcomes = statuses.iter().any(|status| {
                matches!(
                    status,
                    TalkerStatus::Counters {
                        failed_sends,
                        suppressed_sends,
                        ..
                    } if *failed_sends >= 1 && *suppressed_sends >= 1
                )
            });
            if saw_live_outcomes {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed and suppressed counters did not become live"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let errors = statuses
            .iter()
            .filter(|s| matches!(s, TalkerStatus::ConnectionError { .. }))
            .count();
        assert_eq!(
            errors, 1,
            "edge-triggered: only the episode's first failure is reported"
        );
        let outcomes = statuses.iter().rev().find_map(|status| match status {
            TalkerStatus::Counters {
                total_count,
                failed_sends,
                suppressed_sends,
                ..
            } => Some((*total_count, *failed_sends, *suppressed_sends)),
            _ => None,
        });
        let (sent, failed, suppressed) = outcomes.expect("live cumulative counters");
        assert_eq!(sent, 0);
        assert!(failed >= 1, "the failed attempt is visible before stop");
        assert!(
            suppressed >= 1,
            "backoff-suppressed sends are visible before stop"
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
        let (control_tx, control_rx) = crossbeam_channel::bounded(16);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "test".into(),
        };
        let thread = std::thread::spawn(move || {
            run(
                who,
                interface,
                None,
                schedule,
                cmd_rx,
                RunnerObserver::new(status_tx, ObserverPolicy::every_send())
                    .with_control(control_tx),
            )
        });
        let handle = TalkerHandle {
            cmd_tx,
            control_rx,
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
        let final_statuses = handle.status_rx.clone();
        handle.cmd_tx.send(TalkerCommand::Stop).unwrap();
        join_within(handle, Duration::from_secs(2));
        let final_outcomes = final_statuses.try_iter().find_map(|status| match status {
            TalkerStatus::Counters {
                failed_sends,
                suppressed_sends,
                ..
            } => Some((failed_sends, suppressed_sends)),
            _ => None,
        });
        let (failed_total, suppressed_total) = final_outcomes.expect("final cumulative counters");
        assert!(failed_total >= failures);
        assert!(suppressed_total >= suppressed);
    }

    #[test]
    fn open_and_run_reports_open_failed() {
        // Port 1 on loopback refuses immediately on every platform we target.
        let cfg = InterfaceConfig::TcpClient(TcpClientConfig::new("127.0.0.1:1".parse().unwrap()));
        let schedule = Schedule::compile(&[msg("AB", 100)], Instant::now()).unwrap();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::bounded::<TalkerCommand>(8);
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        let who = RunnerIdentity {
            id: ChannelId::mint(),
            label: "3".into(),
        };
        let id = who.id;
        open_and_run(
            who,
            cfg,
            schedule,
            cmd_rx,
            RunnerObserver::new(status_tx, ObserverPolicy::every_send()),
        );
        match status_rx.try_recv() {
            Ok(TalkerStatus::OpenFailed { channel, message }) => {
                assert_eq!(channel, id, "OpenFailed carries the stable id");
                assert!(message.contains("127.0.0.1:1"), "message was: {message}");
            }
            other => panic!("expected OpenFailed, got {:?}", other.is_ok()),
        }
    }
}
