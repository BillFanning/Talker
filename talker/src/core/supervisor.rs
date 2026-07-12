//! Channel supervision shared by the CLI and the GUI (ADR-019).
//!
//! [`TalkerSupervisor`] owns the runner threads, their command/status channel
//! pairs, the draining buckets, and the per-channel observer telemetry that
//! used to live in the GUI module — restoring the spec §2.2 boundary (channel
//! collection and management are business logic, so they live in core; `cli`
//! and `gui` are thin layers over this one API).
//!
//! Threading model is unchanged (ADR-002): no supervisor thread. The owner
//! calls [`poll`](TalkerSupervisor::poll) at its own cadence (the GUI each
//! frame, woken by the notify callback) and the supervisor drains statuses
//! non-blockingly. Runner threads are never joined on the caller's thread —
//! a stopped runner drains in the background and is reaped by `poll`; a slot
//! restart hands the predecessors to the *new* runner thread, which joins
//! them before reopening the interface (a serial port is exclusive — the old
//! holder must drop first).
//!
//! Unlike the old GUI-owned flow, a stopped runner's **status receiver is
//! kept** until its thread exits, so the final `Counters` emitted at stop
//! (ADR-018) still lands and totals read exact at rest.

use std::sync::Arc;

use crossbeam_channel::{Receiver, TrySendError};

use crate::core::channel::{ChannelId, InterfaceConfig};
use crate::core::runner::{
    self, ObserverPolicy, RunnerIdentity, TalkerCommand, TalkerHandle, TalkerStatus,
};
use crate::core::scheduler::Schedule;

/// Bound on each runner's status queue. Occupancy near this cap means
/// observer updates are about to be dropped (and counted) — surfaced as the
/// "Status queue" readout.
pub const STATUS_QUEUE_CAP: usize = 256;

/// Bound on each runner's command queue. Commands are tiny and rare; a full
/// queue means the runner is wedged in a blocking send.
const CMD_QUEUE_CAP: usize = 32;

/// Per-channel observer state, updated by [`TalkerSupervisor::poll`] from the
/// runner's ADR-018 status lanes. Everything cumulative comes straight from
/// `TalkerStatus::Counters`, so it self-corrects across dropped updates.
#[derive(Clone, Debug, Default)]
pub struct ChannelTelemetry {
    /// Running send count across all messages in this channel.
    pub total_count: u64,
    /// Cumulative wire bytes sent.
    pub total_bytes: u64,
    /// Per-message running send counts, indexed by schedule position.
    pub per_message_counts: Vec<u64>,
    /// Status updates the runner discarded because the queue was full.
    pub dropped_statuses: u64,
    /// Sends skipped under the scheduler's stall policy — cadence health.
    pub missed_sends: u64,
    /// Status-queue occupancy sampled at the last poll, and its high-water
    /// mark since the channel started.
    pub queue_len: usize,
    pub queue_peak: usize,
    /// Errors observed since the channel started: connection/open errors
    /// plus undeliverable commands.
    pub errors_total: u64,
    /// The latest **interface** error (connection/open failure). Cleared by a
    /// delivered payload sample or a `SendRecovered` — live proof the
    /// interface works again — and on start.
    pub last_error: Option<String>,
    /// The latest **control-plane** error (an undeliverable Stop/interface-
    /// update/interval command). A healthy sample must NOT clear this — the
    /// wire working says nothing about a command that never arrived. Cleared
    /// only by a later delivered command, or on start.
    pub command_error: Option<String>,
}

impl ChannelTelemetry {
    /// The banner the UI shows: a pending control-plane failure (needs the
    /// user's attention — the on-screen state diverged from the runner's)
    /// wins over an interface error.
    pub fn banner_error(&self) -> Option<&str> {
        self.command_error.as_deref().or(self.last_error.as_deref())
    }
}

/// One sampled send returned by [`TalkerSupervisor::poll`] — the exact wire
/// bytes, for a display pane. Cadence is the runner's [`ObserverPolicy`].
pub struct PayloadSample {
    /// The **current** slot index the sample drained from — the right key for
    /// positional display routing (the supervisor's receivers travel with
    /// their slots, so this is correct even after removals; the status's
    /// embedded stable id serves consumers outside the slot structure).
    pub slot: usize,
    pub payload: Vec<u8>,
}

/// How a command delivery went. `NotRunning` covers both "no runner in this
/// slot" and "the runner already exited" — for a Stop that is moot, for
/// anything else it is surfaced in the telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Delivered,
    /// The command queue is full — the runner may be wedged in a blocking
    /// send. Recorded in the channel's telemetry.
    QueueFull,
    NotRunning,
}

/// A stopped runner still winding down: its thread, plus its status receiver
/// so the final `Counters` (ADR-018) is still drained into the telemetry.
struct DrainingRunner {
    thread: std::thread::JoinHandle<()>,
    status_rx: Receiver<TalkerStatus>,
}

struct Slot {
    /// Stable identity, minted when the slot is created (ADR-020). Slots
    /// shift positionally on removal; the id travels with the slot, and
    /// everything the slot's runners ever emitted is attributed to it.
    id: ChannelId,
    /// Human label frozen at the last start ("3", "'GPS'") — used in the
    /// supervisor's own log text so it matches the runner's. `None` before
    /// the first start (falls back to the id's `#N` form).
    label: Option<String>,
    handle: Option<TalkerHandle>,
    draining: Vec<DrainingRunner>,
    telemetry: ChannelTelemetry,
}

impl Slot {
    fn new() -> Self {
        Self {
            id: ChannelId::mint(),
            label: None,
            handle: None,
            draining: Vec::new(),
            telemetry: ChannelTelemetry::default(),
        }
    }

    /// The label for log text: the start-time label, else the id ("#N").
    fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// The channel collection (spec §2.2): index-stable slots, one per configured
/// channel, mirroring the profile's channel order.
pub struct TalkerSupervisor {
    slots: Vec<Slot>,
    policy: ObserverPolicy,
    /// Cloned into every runner thread's status-notify callback (the GUI
    /// passes its repaint coalescer; the CLI passes nothing).
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Threads of removed slots, still winding down. Reaped by `poll` — the
    /// old GUI flow silently detached these.
    orphans: Vec<DrainingRunner>,
}

impl TalkerSupervisor {
    pub fn new(policy: ObserverPolicy) -> Self {
        Self {
            slots: Vec::new(),
            policy,
            notify: None,
            orphans: Vec::new(),
        }
    }

    /// Install the wake callback cloned into every runner spawned from now
    /// on (the GUI's repaint coalescer).
    pub fn set_notify(&mut self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.notify = Some(notify);
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Append one empty slot (a newly added channel), minting its stable id.
    pub fn push_slot(&mut self) {
        self.slots.push(Slot::new());
    }

    /// Slot `i`'s stable [`ChannelId`] (ADR-020) — the key log-count and
    /// status attribution use; `None` for an out-of-range index.
    pub fn channel_id(&self, i: usize) -> Option<ChannelId> {
        self.slots.get(i).map(|s| s.id)
    }

    /// Remove slot `i`, shifting the ones above it down (mirrors the channel
    /// list). A still-running runner is stopped; its thread joins the orphan
    /// bucket and is reaped by `poll`.
    pub fn remove_slot(&mut self, i: usize) {
        if i >= self.slots.len() {
            return;
        }
        self.stop(i);
        let slot = self.slots.remove(i);
        self.orphans.extend(slot.draining);
    }

    /// Ensure exactly `n` slots exist (profile load), dropping extras via
    /// [`remove_slot`](Self::remove_slot) semantics.
    pub fn resize_slots(&mut self, n: usize) {
        while self.slots.len() > n {
            self.remove_slot(self.slots.len() - 1);
        }
        while self.slots.len() < n {
            self.push_slot();
        }
    }

    pub fn is_running(&self, i: usize) -> bool {
        self.slots.get(i).is_some_and(|s| s.handle.is_some())
    }

    pub fn any_running(&self) -> bool {
        self.slots.iter().any(|s| s.handle.is_some())
    }

    /// Whether any stopped runner is still winding down (blocking send /
    /// interface timeout). Callers keep polling while true so reaping and
    /// tail-draining continue.
    pub fn any_draining(&self) -> bool {
        !self.orphans.is_empty() || self.slots.iter().any(|s| !s.draining.is_empty())
    }

    /// This slot's telemetry (zeroed default for an out-of-range index, so
    /// render code can read unconditionally).
    pub fn telemetry(&self, i: usize) -> ChannelTelemetry {
        self.slots
            .get(i)
            .map(|s| s.telemetry.clone())
            .unwrap_or_default()
    }

    /// Start (or restart) channel `i` with an interface config and a compiled
    /// schedule. `label` is the human name for log text (frozen for the run —
    /// ADR-020; attribution itself rides the slot's stable id). Telemetry
    /// resets; the previous runner (if any) is stopped and handed to the new
    /// thread as a predecessor to join before the interface reopens.
    pub fn start(
        &mut self,
        i: usize,
        label: impl Into<String>,
        cfg: InterfaceConfig,
        schedule: Schedule,
    ) {
        let message_count = schedule.len();
        self.begin_start(i, message_count);
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        let label = label.into();
        slot.label = Some(label.clone());
        let who = RunnerIdentity { id: slot.id, label };
        let predecessors: Vec<_> = std::mem::take(&mut slot.draining)
            .into_iter()
            .map(|d| d.thread)
            .collect();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CMD_QUEUE_CAP);
        let (status_tx, status_rx) = crossbeam_channel::bounded(STATUS_QUEUE_CAP);
        let notify: Option<runner::StatusNotify> = self.notify.clone().map(|n| {
            let f: runner::StatusNotify = Box::new(move || n());
            f
        });
        let policy = self.policy;
        tracing::info!(
            channel = who.id.as_u64(),
            "channel {} starting ({message_count}-message schedule)",
            who.label
        );
        let thread = std::thread::spawn(move || {
            for pred in predecessors {
                let _ = pred.join();
            }
            runner::open_and_run(who, cfg, schedule, cmd_rx, status_tx, notify, policy);
        });
        self.slots[i].handle = Some(TalkerHandle {
            cmd_tx,
            status_rx,
            thread,
        });
    }

    /// Shared start prologue: stop any current runner and zero the telemetry
    /// (per-message counts sized to the new schedule).
    fn begin_start(&mut self, i: usize, message_count: usize) {
        self.stop(i);
        if let Some(slot) = self.slots.get_mut(i) {
            slot.telemetry = ChannelTelemetry {
                per_message_counts: vec![0; message_count],
                ..ChannelTelemetry::default()
            };
        }
    }

    /// Stop channel `i` without blocking: send `Stop` and let the runner
    /// drain in the background (reaped by `poll`; predecessors joined by the
    /// next start of this slot). A runner that already exited makes the stop
    /// moot — only a full command queue is surfaced.
    pub fn stop(&mut self, i: usize) -> CommandOutcome {
        let Some(slot) = self.slots.get_mut(i) else {
            return CommandOutcome::NotRunning;
        };
        let Some(handle) = slot.handle.take() else {
            return CommandOutcome::NotRunning;
        };
        let outcome = match handle.cmd_tx.try_send(TalkerCommand::Stop) {
            Ok(()) => CommandOutcome::Delivered,
            Err(TrySendError::Full(_)) => CommandOutcome::QueueFull,
            Err(TrySendError::Disconnected(_)) => CommandOutcome::NotRunning,
        };
        // Keep the receiver: the final Counters (ADR-018) still lands.
        slot.draining.push(DrainingRunner {
            thread: handle.thread,
            status_rx: handle.status_rx,
        });
        if outcome == CommandOutcome::QueueFull {
            self.record_command_failure(i, "Stop", CommandOutcome::QueueFull);
        }
        if let Some(slot) = self.slots.get(i) {
            tracing::info!(
                channel = slot.id.as_u64(),
                "channel {} stopping",
                slot.display_label()
            );
        }
        outcome
    }

    pub fn stop_all(&mut self) {
        for i in 0..self.slots.len() {
            let _ = self.stop(i);
        }
    }

    /// Reopen channel `i`'s interface with a new configuration, live.
    pub fn update_interface(&mut self, i: usize, cfg: InterfaceConfig) -> CommandOutcome {
        self.send_command(
            i,
            TalkerCommand::UpdateInterface(cfg),
            "the interface update",
        )
    }

    /// Change message `index`'s send interval on channel `i`, live.
    pub fn set_interval(&mut self, i: usize, index: usize, interval_ms: u64) -> CommandOutcome {
        self.send_command(
            i,
            TalkerCommand::SetInterval { index, interval_ms },
            "the interval change",
        )
    }

    fn send_command(&mut self, i: usize, cmd: TalkerCommand, what: &str) -> CommandOutcome {
        let outcome = match self.slots.get(i).and_then(|s| s.handle.as_ref()) {
            Some(h) => match h.cmd_tx.try_send(cmd) {
                Ok(()) => CommandOutcome::Delivered,
                Err(TrySendError::Full(_)) => CommandOutcome::QueueFull,
                Err(TrySendError::Disconnected(_)) => CommandOutcome::NotRunning,
            },
            None => CommandOutcome::NotRunning,
        };
        match outcome {
            CommandOutcome::Delivered => {
                // A delivered command supersedes a pending control-plane
                // failure — the divergence the banner warned about is over.
                if let Some(slot) = self.slots.get_mut(i) {
                    slot.telemetry.command_error = None;
                }
            }
            _ => self.record_command_failure(i, what, outcome),
        }
        outcome
    }

    /// An undeliverable command makes the on-screen state diverge from the
    /// runner's — to the user it looks like a no-op bug, so it lands in the
    /// channel's error telemetry, not just the log.
    fn record_command_failure(&mut self, i: usize, what: &str, outcome: CommandOutcome) {
        let why = match outcome {
            CommandOutcome::QueueFull => {
                "the runner's command queue is full (it may be wedged in a blocking send)"
            }
            _ => "the runner has already exited",
        };
        let msg = format!("{what} was not delivered: {why}");
        if let Some(slot) = self.slots.get(i) {
            tracing::warn!(
                channel = slot.id.as_u64(),
                "channel {}: {msg}",
                slot.display_label()
            );
        }
        if let Some(slot) = self.slots.get_mut(i) {
            slot.telemetry.errors_total += 1;
            // Control-plane class: a healthy payload sample must not clear
            // this (the wire working says nothing about the lost command).
            slot.telemetry.command_error = Some(msg);
        }
    }

    /// Join every runner thread — running, draining, and orphaned.
    /// **Blocks**, bounded by the interfaces' send timeouts; exit path only,
    /// so serial ports and sockets close cleanly before the process dies
    /// instead of being killed mid-write.
    ///
    /// Safe on live handles: the command sender is dropped *before* the join,
    /// so a runner that never received Stop still exits on the disconnect
    /// (joining with the sender alive would deadlock — the runner would keep
    /// waiting for commands forever). Status receivers are drained first so a
    /// runner block-sending its final `Counters` can always complete.
    pub fn join_all(&mut self) {
        for slot in &mut self.slots {
            if let Some(h) = slot.handle.take() {
                let TalkerHandle {
                    cmd_tx,
                    status_rx,
                    thread,
                } = h;
                drop(cmd_tx);
                for _ in status_rx.try_iter() {}
                let _ = thread.join();
            }
            for d in slot.draining.drain(..) {
                for _ in d.status_rx.try_iter() {}
                let _ = d.thread.join();
            }
        }
        for d in self.orphans.drain(..) {
            for _ in d.status_rx.try_iter() {}
            let _ = d.thread.join();
        }
    }

    /// Drain every runner's status queue into the telemetry, reap finished
    /// threads (running, draining, and orphaned), and return the payload
    /// samples for the display pane. Non-blocking; call at the UI cadence.
    pub fn poll(&mut self) -> Vec<PayloadSample> {
        let mut samples = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(h) = &slot.handle {
                // Sample occupancy *before* draining so the peak reflects the
                // backlog as it stood, then drain. Finished check comes
                // before the drain: if the thread was already done, the drain
                // below is guaranteed complete (the sender is gone).
                let finished = h.thread.is_finished();
                let qlen = h.status_rx.len();
                slot.telemetry.queue_len = qlen;
                slot.telemetry.queue_peak = slot.telemetry.queue_peak.max(qlen);
                drain_statuses(i, &h.status_rx, &mut slot.telemetry, &mut samples);
                if finished {
                    slot.handle = None; // runner exited on its own (open failed / disconnect)
                }
            }
            // Draining runners: keep collecting their tail (the final
            // Counters) until the thread exits. Finished check BEFORE the
            // drain — finished first guarantees the queue already holds
            // everything the runner ever sent, so nothing is lost when the
            // entry is dropped.
            slot.draining.retain_mut(|d| {
                let finished = d.thread.is_finished();
                drain_statuses(i, &d.status_rx, &mut slot.telemetry, &mut samples);
                !finished
            });
        }
        self.orphans.retain(|d| !d.thread.is_finished());
        samples
    }
}

/// Fold one receiver's pending statuses into `telemetry`, collecting payload
/// samples tagged with the slot the receiver currently occupies. Shared by
/// the live and draining paths.
fn drain_statuses(
    slot: usize,
    status_rx: &Receiver<TalkerStatus>,
    telemetry: &mut ChannelTelemetry,
    samples: &mut Vec<PayloadSample>,
) {
    for status in status_rx.try_iter() {
        match status {
            TalkerStatus::Counters {
                total_count,
                total_bytes,
                per_message_counts,
                dropped_statuses,
                missed_sends,
                ..
            } => {
                telemetry.total_count = total_count;
                telemetry.total_bytes = total_bytes;
                telemetry.per_message_counts = per_message_counts;
                telemetry.dropped_statuses = dropped_statuses;
                telemetry.missed_sends = missed_sends;
            }
            TalkerStatus::SendSample { payload, .. } => {
                // Live proof of a working interface: clear the banner. During
                // a failing episode sends are suppressed, so no samples
                // arrive and the banner correctly persists.
                telemetry.last_error = None;
                samples.push(PayloadSample { slot, payload });
            }
            TalkerStatus::ConnectionError { message, .. }
            | TalkerStatus::OpenFailed { message, .. } => {
                telemetry.errors_total += 1;
                telemetry.last_error = Some(message);
            }
            TalkerStatus::SendRecovered { .. } => {
                telemetry.last_error = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::core::channel::Interface;
    use crate::core::message::{MessageConfig, PayloadConfig};

    /// Start slot `i` on a caller-supplied interface (no real I/O), through
    /// the same prologue/spawn shape as [`TalkerSupervisor::start`].
    fn start_with_interface(
        sup: &mut TalkerSupervisor,
        i: usize,
        interface: Box<dyn Interface>,
        schedule: Schedule,
    ) {
        sup.begin_start(i, schedule.len());
        let slot = sup.slots.get_mut(i).expect("slot exists");
        let who = RunnerIdentity {
            id: slot.id,
            label: format!("{}", i + 1),
        };
        let predecessors: Vec<_> = std::mem::take(&mut slot.draining)
            .into_iter()
            .map(|d| d.thread)
            .collect();
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CMD_QUEUE_CAP);
        let (status_tx, status_rx) = crossbeam_channel::bounded(STATUS_QUEUE_CAP);
        let policy = sup.policy;
        let thread = std::thread::spawn(move || {
            for pred in predecessors {
                let _ = pred.join();
            }
            runner::run(who, interface, schedule, cmd_rx, status_tx, None, policy);
        });
        sup.slots[i].handle = Some(TalkerHandle {
            cmd_tx,
            status_rx,
            thread,
        });
    }

    struct CountingInterface {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Interface for CountingInterface {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn msg(hex: &str, interval_ms: u64) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex(hex), interval_ms)
    }

    fn schedule(messages: &[MessageConfig]) -> Schedule {
        Schedule::compile(messages, Instant::now()).unwrap()
    }

    fn poll_until(
        sup: &mut TalkerSupervisor,
        samples: &mut Vec<PayloadSample>,
        mut done: impl FnMut(&TalkerSupervisor) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(sup) {
            assert!(Instant::now() < deadline, "condition not reached in 5 s");
            samples.extend(sup.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn start_poll_stop_reaps_and_reads_exact_totals_at_rest() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        assert!(sup.is_running(0));

        // Wait for a few sends to land in the telemetry.
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 3);

        assert_eq!(sup.stop(0), CommandOutcome::Delivered);
        assert!(!sup.is_running(0));

        // The draining runner's tail (final Counters) is still collected, so
        // once fully reaped the telemetry equals the wire exactly.
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
        let telemetry = sup.telemetry(0);
        let wire = sent.lock().unwrap().len() as u64;
        assert_eq!(telemetry.total_count, wire, "totals exact at rest");
        // every_send policy: one sample per send reached the display lane.
        assert_eq!(samples.len() as u64, wire);
        assert!(samples.iter().all(|s| s.slot == 0));
    }

    #[test]
    fn commands_on_a_stopped_slot_report_not_running_and_surface_it() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        assert_eq!(sup.stop(0), CommandOutcome::NotRunning);
        // A moot Stop is silent…
        assert!(sup.telemetry(0).last_error.is_none());
        // …but a lost interval change is surfaced (§ the on-screen state
        // would silently diverge otherwise).
        assert_eq!(sup.set_interval(0, 0, 50), CommandOutcome::NotRunning);
        let t = sup.telemetry(0);
        assert_eq!(t.errors_total, 1);
        assert!(t
            .command_error
            .as_deref()
            .unwrap()
            .contains("interval change"));
        assert!(t.banner_error().is_some());
    }

    /// Error-class separation: a healthy payload sample clears an *interface*
    /// error but must NOT clear a *control-plane* one — the wire working says
    /// nothing about a command that never arrived.
    #[test]
    fn samples_clear_interface_errors_but_not_command_errors() {
        let mut telemetry = ChannelTelemetry {
            last_error: Some("send failed".into()),
            command_error: Some("the interval change was not delivered".into()),
            ..ChannelTelemetry::default()
        };
        let (tx, rx) = crossbeam_channel::bounded(4);
        tx.send(TalkerStatus::SendSample {
            channel: ChannelId::mint(),
            message_index: 0,
            payload: vec![0xAB],
        })
        .unwrap();
        drop(tx);
        let mut samples = Vec::new();
        drain_statuses(0, &rx, &mut telemetry, &mut samples);
        assert!(telemetry.last_error.is_none(), "interface error cleared");
        assert!(
            telemetry.command_error.is_some(),
            "control-plane error survives a healthy sample"
        );
        assert_eq!(samples.len(), 1);
    }

    #[test]
    fn restart_resets_telemetry_and_joins_predecessors() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 2);

        // Restart without an explicit stop: start() stops the old runner and
        // the new thread joins it before running.
        start_with_interface(
            &mut sup,
            0,
            Box::new(CountingInterface {
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            schedule(&[msg("CD", 5), msg("EF", 5)]),
        );
        // Telemetry was reset for the new two-message schedule.
        assert_eq!(sup.telemetry(0).total_count, 0);
        assert_eq!(sup.telemetry(0).per_message_counts.len(), 2);

        poll_until(&mut sup, &mut samples, |s| s.telemetry(0).total_count >= 2);
        sup.stop_all();
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }

    #[test]
    fn channel_ids_are_stable_across_slot_removal() {
        // The whole point of ADR-020: positions shift, identity doesn't. A
        // running runner keeps stamping the id its slot was minted with, so
        // log-count attribution keyed by id can never land on the wrong row.
        let mut sup = TalkerSupervisor::new(ObserverPolicy::sampled());
        sup.push_slot();
        sup.push_slot();
        let first = sup.channel_id(0).unwrap();
        let second = sup.channel_id(1).unwrap();
        assert_ne!(first, second);

        sup.remove_slot(0);
        assert_eq!(
            sup.channel_id(0),
            Some(second),
            "the surviving slot keeps its id after shifting down"
        );

        // A fresh slot mints a fresh id — removed ids are never reused.
        sup.push_slot();
        let third = sup.channel_id(1).unwrap();
        assert_ne!(third, first);
        assert_ne!(third, second);
    }

    #[test]
    fn remove_slot_shifts_telemetry_and_orphans_the_runner() {
        let mut sup = TalkerSupervisor::new(ObserverPolicy::every_send());
        sup.push_slot();
        sup.push_slot();
        let sent = Arc::new(Mutex::new(Vec::new()));
        start_with_interface(
            &mut sup,
            1,
            Box::new(CountingInterface {
                sent: Arc::clone(&sent),
            }),
            schedule(&[msg("AB", 5)]),
        );
        let mut samples = Vec::new();
        poll_until(&mut sup, &mut samples, |s| s.telemetry(1).total_count >= 1);

        sup.remove_slot(0);
        assert_eq!(sup.len(), 1);
        // Slot 1's telemetry shifted down to index 0.
        assert!(sup.telemetry(0).total_count >= 1);
        // Removing the (shifted) running slot orphans its runner; poll reaps.
        sup.remove_slot(0);
        assert_eq!(sup.len(), 0);
        poll_until(&mut sup, &mut samples, |s| !s.any_draining());
    }
}
